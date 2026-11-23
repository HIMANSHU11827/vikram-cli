// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo
//! GGUF v3 reader: metadata from the header, and F32/F16/Q8_0 and the K-quants (Q4_K, Q5_K, Q6_K)
//! streamed from the file. F32 and F16 are used as they are; Q8_0 and the K-quants are dequantized
//! block by block, either on the way to a kernel that reads them quantized (the GPU's Q8_0 path,
//! which takes the 34-byte blocks) or into f16/f32 when a caller asks for floats.

use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use half::f16;

use crate::error::{Result, bail};

/// Values in a K-quant super-block.
const QK_K: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(non_camel_case_types)] // The variants keep ggml's type names.
pub enum Type {
  F32,
  F16,
  /// Blocks of 32 weights: f16 scale + 32 × i8 (34 bytes).
  Q8_0,
  /// Super-blocks of 256 weights: f16 scale `d`, f16 min `dmin`, eight 6-bit scales and eight
  /// 6-bit mins (12 bytes), then 256 4-bit quants (128 bytes): 144 bytes.
  Q4_K,
  /// As Q4_K, with the quants' fifth bit in 32 bytes before the 128 of low nibbles: 176 bytes.
  Q5_K,
  /// Super-blocks of 256 weights: 256 6-bit quants (128 low + 64 high bytes), 16 × i8 scales,
  /// f16 scale `d`: 210 bytes. No mins, as 6 bits keep the range symmetric.
  Q6_K,
}

impl Type {
  fn from_ggml(id: u32) -> Option<Self> {
    match id {
      0 => Some(Self::F32),
      1 => Some(Self::F16),
      8 => Some(Self::Q8_0),
      12 => Some(Self::Q4_K),
      13 => Some(Self::Q5_K),
      14 => Some(Self::Q6_K),
      _ => None,
    }
  }

  /// Values per block: a tensor is a whole number of blocks.
  pub(crate) fn block(self) -> usize {
    match self {
      Self::F32 | Self::F16 => 1,
      Self::Q8_0 => 32,
      Self::Q4_K | Self::Q5_K | Self::Q6_K => QK_K,
    }
  }

  /// Bytes of a block: an f32 or f16 value, or a quantized block.
  pub(crate) fn block_bytes(self) -> usize {
    match self {
      Self::F32 => 4,
      Self::F16 => 2,
      Self::Q8_0 => 34,
      Self::Q4_K => 144,
      Self::Q5_K => 176,
      Self::Q6_K => 210,
    }
  }

  fn size(self, n: usize) -> usize {
    n / self.block() * self.block_bytes()
  }

  /// Dequantizes the block `b` (`block_bytes` bytes) into `out` (`block()` values).
  pub(crate) fn dequant(self, b: &[u8], out: &mut [f32]) {
    match self {
      Self::F32 => out[0] = f32::from_le_bytes(b.try_into().unwrap()),
      Self::F16 => out[0] = f16::from_le_bytes(b.try_into().unwrap()).to_f32(),
      Self::Q8_0 => {
        let d = f16::from_le_bytes([b[0], b[1]]).to_f32();
        out.iter_mut().zip(&b[2..]).for_each(|(o, &q)| *o = q as i8 as f32 * d);
      }
      Self::Q4_K => q4_k(b, out),
      Self::Q5_K => q5_k(b, out),
      Self::Q6_K => q6_k(b, out),
    }
  }
}

/// A metadata value, decoded from the header when a model reads it.
#[derive(Debug, Clone, Copy)]
pub enum Value<'a> {
  /// Any integer type: no model reads a u64 above `i64::MAX`.
  Int(i64),
  Float(f64),
  Bool(bool),
  /// `None` when not UTF-8.
  Str(Option<&'a str>),
  /// Items of GGUF type `item`, read by `Gguf::array`.
  Array {
    item: u32,
    len: usize,
  },
}

/// A metadata type that models read: `u32`, `i32`, `f32`, `bool` or `&str`.
pub trait Meta<'a>: Sized {
  fn from(v: Value<'a>) -> Option<Self>;
}

impl Meta<'_> for u32 {
  fn from(v: Value) -> Option<Self> {
    if let Value::Int(v) = v { v.try_into().ok() } else { None }
  }
}

impl Meta<'_> for i32 {
  fn from(v: Value) -> Option<Self> {
    if let Value::Int(v) = v { v.try_into().ok() } else { None }
  }
}

impl Meta<'_> for f32 {
  fn from(v: Value) -> Option<Self> {
    if let Value::Float(v) = v { Some(v as f32) } else { None }
  }
}

impl Meta<'_> for bool {
  fn from(v: Value) -> Option<Self> {
    if let Value::Bool(v) = v { Some(v) } else { None }
  }
}

impl<'a> Meta<'a> for &'a str {
  fn from(v: Value<'a>) -> Option<Self> {
    if let Value::Str(s) = v { s } else { None }
  }
}

struct Info {
  dims: Vec<usize>,
  ty: Type,
  offset: usize,
}

pub struct Gguf {
  path: PathBuf,
  /// The start of the file, at least the header; tensors inside it are read from here.
  head: Vec<u8>,
  /// Per key the value's GGUF type and its offset in `head`. Values are decoded when read, so a
  /// vocabulary nobody reads costs nothing.
  meta: HashMap<String, (u32, usize)>,
  tensors: HashMap<String, Info>,
  data: usize,
}

/// A tensor in the file. `dims` are in ggml order: `dims[0]` is the contiguous axis, so a linear
/// weight `[in, out]` is row-major `[out][in]`.
pub struct Tensor<'a> {
  pub dims: &'a [usize],
  pub ty: Type,
  gguf: &'a Gguf,
  start: usize,
  size: usize,
}

impl Gguf {
  /// Reads the header only; tensor data is read when used.
  pub fn open(path: &Path) -> Result<Self> {
    let len = std::fs::metadata(path)?.len() as usize;
    // The header is usually far below 1 MiB; read more of the file if it doesn't fit.
    let mut n = len.min(1 << 20);
    loop {
      let mut head = vec![0; n];
      File::open(path)?.read_exact(&mut head)?;
      match Self::parse(head, len, path.into()) {
        Err(_) if n < len => n = len.min(n * 4),
        r => return r,
      }
    }
  }

  fn parse(head: Vec<u8>, len: usize, path: PathBuf) -> Result<Self> {
    let mut r = Reader { bytes: &head, pos: 0 };
    if r.take(4)? != b"GGUF" {
      bail!("not a GGUF file");
    }
    let version = r.u32()?;
    if version != 3 {
      bail!("unsupported GGUF version {version}");
    }
    let n_tensors = r.u64()? as usize;
    let n_meta = r.u64()? as usize;

    let mut meta = HashMap::new();
    for _ in 0..n_meta {
      let key = r.string()?;
      let ty = r.u32()?;
      meta.insert(key, (ty, r.pos));
      r.skip(ty)?;
    }

    let mut tensors = HashMap::new();
    for _ in 0..n_tensors {
      let name = r.string()?;
      let n_dims = r.u32()? as usize;
      let dims = (0..n_dims).map(|_| r.u64().map(|d| d as usize)).collect::<Result<Vec<_>>>()?;
      let id = r.u32()?;
      let Some(ty) = Type::from_ggml(id) else {
        bail!("tensor {name}: unsupported ggml type {id}")
      };
      let n: usize = dims.iter().product();
      if !n.is_multiple_of(ty.block()) {
        bail!("tensor {name}: {n} values is not a whole number of {} blocks", ty.block());
      }
      let offset = r.u64()? as usize;
      tensors.insert(name, Info { dims, ty, offset });
    }

    let end = r.pos;
    let mut g = Self { path, head, meta, tensors, data: 0 };
    let align = match g.value("general.alignment") {
      Ok((_, Value::Int(a))) => a as usize,
      _ => 32,
    };
    g.data = end.next_multiple_of(align);
    for (name, t) in &g.tensors {
      if g.data + t.offset + t.ty.size(t.dims.iter().product()) > len {
        bail!("tensor {name} is out of file bounds");
      }
    }
    Ok(g)
  }

  /// The value of `key`, and a reader past it (at an array's items).
  fn value(&self, key: &str) -> Result<(Reader<'_>, Value<'_>)> {
    let Some(&(ty, pos)) = self.meta.get(key) else { bail!("missing metadata {key}") };
    let mut r = Reader { bytes: &self.head, pos };
    let v = r.value(ty)?;
    Ok((r, v))
  }

  /// The metadata value `key` as a `T`.
  pub fn get<'a, T: Meta<'a>>(&'a self, key: &str) -> Result<T> {
    let (_, v) = self.value(key)?;
    match T::from(v) {
      Some(v) => Ok(v),
      None => bail!("metadata {key}: expected {}, got {v:?}", std::any::type_name::<T>()),
    }
  }

  /// The metadata value `key` as a `T`, or `None` when the file has no `key`.
  pub fn optional<'a, T: Meta<'a>>(&'a self, key: &str) -> Result<Option<T>> {
    if !self.meta.contains_key(key) {
      return Ok(None);
    }
    self.get(key).map(Some)
  }

  /// Fails unless the metadata value `key` is one of `allowed`.
  pub fn check<'a, T: Meta<'a> + PartialEq + Debug>(
    &'a self,
    key: &str,
    allowed: &[T],
  ) -> Result<()> {
    let got = self.get::<T>(key)?;
    if !allowed.contains(&got) {
      bail!("{key} is {got:?}, only {allowed:?} supported");
    }
    Ok(())
  }

  /// The metadata array `key` as `T`s.
  pub fn array<'a, T: Meta<'a>>(&'a self, key: &str) -> Result<Vec<T>> {
    let (mut r, Value::Array { item, len }) = self.value(key)? else {
      bail!("metadata {key}: expected an array")
    };
    let items = (0..len).map(|_| r.value(item).map(T::from)).collect::<Result<Option<_>>>()?;
    let Some(items) = items else {
      bail!("metadata {key}: expected an array of {}", std::any::type_name::<T>())
    };
    Ok(items)
  }

  /// The metadata array `key` as `T`s, empty when the file has no `key`.
  pub fn optional_array<'a, T: Meta<'a>>(&'a self, key: &str) -> Result<Vec<T>> {
    if !self.meta.contains_key(key) {
      return Ok(Vec::new());
    }
    self.array(key)
  }

  /// The tensor `name`, which must have exactly the ggml-order `dims`.
  pub fn tensor(&self, name: &str, dims: &[usize]) -> Result<Tensor<'_>> {
    let Some(t) = self.tensors.get(name) else { bail!("missing tensor {name}") };
    if t.dims != dims {
      bail!("tensor {name}: shape {:?}, expected {dims:?}", t.dims);
    }
    let (start, size) = (self.data + t.offset, t.ty.size(dims.iter().product()));
    Ok(Tensor { dims: &t.dims, ty: t.ty, gguf: self, start, size })
  }
}

impl Tensor<'_> {
  pub fn len(&self) -> usize {
    self.dims.iter().product()
  }

  /// Calls `f(offset, bytes)` on consecutive pieces of the tensor's data of up to `chunk` bytes.
  pub fn read(&self, chunk: usize, mut f: impl FnMut(usize, &[u8])) -> Result<()> {
    if let Some(bytes) = self.gguf.head.get(self.start..self.start + self.size) {
      bytes.chunks(chunk).enumerate().for_each(|(i, c)| f(i * chunk, c));
      return Ok(());
    }
    let mut file = File::open(&self.gguf.path)?;
    file.seek(SeekFrom::Start(self.start as u64))?;
    let mut buf = vec![0; chunk.min(self.size)];
    for offset in (0..self.size).step_by(chunk) {
      let piece = &mut buf[..chunk.min(self.size - offset)];
      file.read_exact(piece)?;
      f(offset, piece);
    }
    Ok(())
  }

  /// The tensor's values, dequantized a block at a time.
  pub fn to_f32(&self) -> Result<Vec<f32>> {
    let mut out = vec![0f32; self.len()];
    self.dequant_into(|at, values| out[at..at + values.len()].copy_from_slice(values))?;
    Ok(out)
  }

  /// The tensor's values as little-endian f16: half the memory of [`Tensor::to_f32`], for weights
  /// a kernel reads as halves anyway.
  pub fn to_f16(&self) -> Result<Vec<u8>> {
    let mut out = vec![0u8; self.len() * 2];
    self.dequant_into(|at, values| {
      let at = 2 * at;
      for (i, v) in values.iter().enumerate() {
        out[at + 2 * i..][..2].copy_from_slice(&f16::from_f32(*v).to_le_bytes());
      }
    })?;
    Ok(out)
  }

  /// Dequantizes the tensor block by block, handing `f(first value, values)` the whole blocks of
  /// one piece: a block never straddles two calls.
  fn dequant_into(&self, mut f: impl FnMut(usize, &[f32])) -> Result<()> {
    let (block, n) = (self.ty.block_bytes(), self.ty.block());
    let mut buf = vec![0f32; n];
    let mut at = 0;
    // Whole blocks per piece, as many as a read chunk of the size the GPU uploads with.
    self.read(block * (34 << 13), |_, piece| {
      for b in piece.chunks_exact(block) {
        self.ty.dequant(b, &mut buf);
        f(at, &buf);
        at += n;
      }
    })
  }
}

/// Dequantizes row `r` of `k` values of a tensor of `ty` whose raw blocks are `blocks` into `out`
/// (`k` values): one row at a time, so no caller holds the whole matrix as floats.
pub(crate) fn dequant_row(ty: Type, blocks: &[u8], r: usize, k: usize, out: &mut [f32]) {
  let (block, n) = (ty.block_bytes(), ty.block());
  let row = &blocks[r * (k / n * block)..][..k / n * block];
  row.chunks_exact(block).zip(out.chunks_mut(n)).for_each(|(b, out)| ty.dequant(b, out));
}

/// The 6-bit scale and min of sub-block `j` of a K-quant, as llama.cpp packs them in 12 bytes:
/// the first four are the low 6 bits of a byte each, the last four the low nibble of a byte and
/// the top two bits of an earlier one.
fn scale_min_k4(j: usize, q: &[u8]) -> (u8, u8) {
  if j < 4 {
    (q[j] & 63, q[j + 4] & 63)
  } else {
    (q[j + 4] & 0xF | (q[j - 4] >> 6) << 4, q[j + 4] >> 4 | (q[j] >> 6) << 4)
  }
}

/// 256 weights from one 144-byte block of Q4_K: 8 sub-blocks of 32, each `d·s·q - dmin·m`.
fn q4_k(b: &[u8], out: &mut [f32]) {
  let d = f16::from_le_bytes([b[0], b[1]]).to_f32();
  let dmin = f16::from_le_bytes([b[2], b[3]]).to_f32();
  let (scales, qs) = (&b[4..16], &b[16..144]);
  for (c, (q, out)) in qs.chunks_exact(32).zip(out.chunks_mut(64)).enumerate() {
    let (s0, m0) = scale_min_k4(2 * c, scales);
    let (s1, m1) = scale_min_k4(2 * c + 1, scales);
    let (d0, o0) = (d * s0 as f32, dmin * m0 as f32);
    let (d1, o1) = (d * s1 as f32, dmin * m1 as f32);
    let (lo, hi) = out.split_at_mut(32);
    for ((i, j), &q) in lo.iter_mut().zip(hi.iter_mut()).zip(q) {
      *i = d0 * (q & 0xF) as f32 - o0;
      *j = d1 * (q >> 4) as f32 - o1;
    }
  }
}

/// 256 weights from one 176-byte block of Q5_K, as Q4_K with a fifth bit per quant.
fn q5_k(b: &[u8], out: &mut [f32]) {
  let d = f16::from_le_bytes([b[0], b[1]]).to_f32();
  let dmin = f16::from_le_bytes([b[2], b[3]]).to_f32();
  let (scales, qh, qs) = (&b[4..16], &b[16..48], &b[48..176]);
  for (c, (q, out)) in qs.chunks_exact(32).zip(out.chunks_mut(64)).enumerate() {
    let (s0, m0) = scale_min_k4(2 * c, scales);
    let (s1, m1) = scale_min_k4(2 * c + 1, scales);
    let (d0, o0) = (d * s0 as f32, dmin * m0 as f32);
    let (d1, o1) = (d * s1 as f32, dmin * m1 as f32);
    let (lo, hi) = out.split_at_mut(32);
    for (((i, j), &q), &h) in lo.iter_mut().zip(hi.iter_mut()).zip(q).zip(qh) {
      let bit = 1 << (2 * c);
      *i = d0 * ((q & 0xF) + if h & bit != 0 { 16 } else { 0 }) as f32 - o0;
      *j = d1 * ((q >> 4) + if h & (bit << 1) != 0 { 16 } else { 0 }) as f32 - o1;
    }
  }
}

/// 256 weights from one 210-byte block of Q6_K: 16 sub-blocks of 16, each `d·s·(q - 32)`.
fn q6_k(b: &[u8], out: &mut [f32]) {
  let d = f16::from_le_bytes([b[208], b[209]]).to_f32();
  let (ql, qh, sc) = (&b[..128], &b[128..192], &b[192..208]);
  for n in 0..2 {
    let (ql, qh, sc) = (&ql[64 * n..][..64], &qh[32 * n..][..32], &sc[8 * n..][..8]);
    let out = &mut out[128 * n..][..128];
    for l in 0..32 {
      let is = l / 16;
      let (h, l0, l1) = (qh[l], ql[l], ql[l + 32]);
      let q = |lo: u8, shift: u32| ((lo & 0xF) | ((h >> shift) & 3) << 4) as f32 - 32.0;
      // The scales are signed: a sub-block of mostly negative weights has a negative one.
      let sc = |i: usize| sc[i] as i8 as f32;
      out[l] = d * sc(is) * q(l0, 0);
      out[l + 32] = d * sc(is + 2) * q(l1, 2);
      out[l + 64] = d * sc(is + 4) * q(l0 >> 4, 4);
      out[l + 96] = d * sc(is + 6) * q(l1 >> 4, 6);
    }
  }
}

#[cfg(test)]
impl Gguf {
  /// An in-memory file holding `(name, ggml dims, type, data)` tensors, for tests.
  pub(crate) fn with_tensors(tensors: &[(&str, &[usize], Type, &[u8])]) -> Self {
    let mut head = Vec::new();
    let mut infos = HashMap::new();
    for &(name, dims, ty, data) in tensors {
      infos.insert(name.to_string(), Info { dims: dims.to_vec(), ty, offset: head.len() });
      head.extend_from_slice(data);
    }
    Self { path: PathBuf::new(), head, meta: HashMap::new(), tensors: infos, data: 0 }
  }
}

struct Reader<'a> {
  bytes: &'a [u8],
  pos: usize,
}

impl<'a> Reader<'a> {
  fn take(&mut self, n: usize) -> Result<&'a [u8]> {
    let Some(s) = self.bytes.get(self.pos..self.pos + n) else { bail!("truncated GGUF header") };
    self.pos += n;
    Ok(s)
  }

  fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
    Ok(self.take(N)?.try_into().unwrap())
  }

  fn u32(&mut self) -> Result<u32> {
    Ok(u32::from_le_bytes(self.array()?))
  }

  fn u64(&mut self) -> Result<u64> {
    Ok(u64::from_le_bytes(self.array()?))
  }

  fn bytes(&mut self) -> Result<&'a [u8]> {
    let n = self.u64()? as usize;
    self.take(n)
  }

  fn string(&mut self) -> Result<String> {
    Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
  }

  /// Moves past a value of type `ty`.
  fn skip(&mut self, ty: u32) -> Result<()> {
    let size = |ty| match ty {
      0 | 1 | 7 => Some(1),
      2 | 3 => Some(2),
      4..=6 => Some(4),
      10..=12 => Some(8),
      _ => None,
    };
    match ty {
      8 => _ = self.bytes()?,
      9 => {
        let item = self.u32()?;
        let n = self.u64()? as usize;
        match size(item) {
          Some(size) => _ = self.take(n * size)?,
          None => (0..n).try_for_each(|_| self.skip(item))?,
        }
      }
      _ => match size(ty) {
        Some(size) => _ = self.take(size)?,
        None => bail!("unknown GGUF value type {ty}"),
      },
    }
    Ok(())
  }

  /// Reads a value of type `ty`; for an array its item type and length, the items follow.
  fn value(&mut self, ty: u32) -> Result<Value<'a>> {
    Ok(match ty {
      0 => Value::Int(self.array::<1>()?[0] as i64),
      1 => Value::Int(self.array::<1>()?[0] as i8 as i64),
      2 => Value::Int(u16::from_le_bytes(self.array()?) as i64),
      3 => Value::Int(i16::from_le_bytes(self.array()?) as i64),
      4 => Value::Int(self.u32()? as i64),
      5 => Value::Int(i32::from_le_bytes(self.array()?) as i64),
      6 => Value::Float(f32::from_le_bytes(self.array()?) as f64),
      7 => Value::Bool(self.array::<1>()?[0] != 0),
      8 => Value::Str(std::str::from_utf8(self.bytes()?).ok()),
      9 => Value::Array { item: self.u32()?, len: self.u64()? as usize },
      10 => Value::Int(self.u64()? as i64),
      11 => Value::Int(i64::from_le_bytes(self.array()?)),
      12 => Value::Float(f64::from_le_bytes(self.array()?)),
      _ => bail!("unknown GGUF value type {ty}"),
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn string(out: &mut Vec<u8>, s: &str) {
    out.extend((s.len() as u64).to_le_bytes());
    out.extend(s.as_bytes());
  }

  /// A file with a u32, a string array, a bool, a negative i32, an i32 array and an f32, and an
  /// F32 + a Q8_0 tensor.
  fn sample() -> Vec<u8> {
    let mut f = b"GGUF".to_vec();
    f.extend(3u32.to_le_bytes());
    f.extend(2u64.to_le_bytes());
    f.extend(6u64.to_le_bytes());
    string(&mut f, "a.n");
    f.extend(4u32.to_le_bytes());
    f.extend(7u32.to_le_bytes());
    string(&mut f, "a.tokens");
    f.extend(9u32.to_le_bytes());
    f.extend(8u32.to_le_bytes());
    f.extend(2u64.to_le_bytes());
    string(&mut f, "▁x");
    string(&mut f, "y");
    string(&mut f, "a.flag");
    f.extend(7u32.to_le_bytes());
    f.push(1);
    string(&mut f, "a.left");
    f.extend(5u32.to_le_bytes());
    f.extend((-1i32).to_le_bytes());
    string(&mut f, "a.durations");
    f.extend(9u32.to_le_bytes());
    f.extend(5u32.to_le_bytes());
    f.extend(3u64.to_le_bytes());
    [0i32, 2, -4].iter().for_each(|v| f.extend(v.to_le_bytes()));
    string(&mut f, "a.alpha");
    f.extend(6u32.to_le_bytes());
    f.extend(0.97f32.to_le_bytes());

    string(&mut f, "w");
    f.extend(2u32.to_le_bytes());
    f.extend(2u64.to_le_bytes());
    f.extend(1u64.to_le_bytes());
    f.extend(0u32.to_le_bytes());
    f.extend(0u64.to_le_bytes());
    string(&mut f, "q");
    f.extend(1u32.to_le_bytes());
    f.extend(32u64.to_le_bytes());
    f.extend(8u32.to_le_bytes());
    f.extend(32u64.to_le_bytes());

    f.resize(f.len().next_multiple_of(32), 0);
    f.extend(1.5f32.to_le_bytes());
    f.extend((-2.0f32).to_le_bytes());
    f.resize(f.len().next_multiple_of(32), 0);
    f.extend(f16::from_f32(0.5).to_le_bytes());
    f.extend((0..32).map(|i| (i as i8 - 16) as u8));
    f
  }

  fn parse(f: Vec<u8>) -> Result<Gguf> {
    let len = f.len();
    Gguf::parse(f, len, PathBuf::new())
  }

  #[test]
  fn reads_metadata_and_tensors() {
    let g = parse(sample()).unwrap();
    assert_eq!(g.get::<u32>("a.n").unwrap(), 7);
    assert_eq!(g.get::<i32>("a.n").unwrap(), 7);
    assert!(g.get::<u32>("missing").is_err());
    assert!(g.get::<&str>("a.n").is_err());
    assert_eq!(g.array::<&str>("a.tokens").unwrap(), ["▁x", "y"]);
    assert!(g.get::<bool>("a.flag").unwrap());
    assert_eq!(g.optional::<bool>("a.flag").unwrap(), Some(true));
    assert_eq!(g.optional::<bool>("missing").unwrap(), None);
    assert!(g.optional::<&str>("a.flag").is_err());
    assert_eq!(g.get::<i32>("a.left").unwrap(), -1);
    assert!(g.get::<u32>("a.left").is_err());
    assert_eq!(g.array::<i32>("a.durations").unwrap(), [0, 2, -4]);
    assert!(g.array::<u32>("a.durations").is_err());
    assert_eq!(g.optional_array::<i32>("a.durations").unwrap(), [0, 2, -4]);
    assert!(g.optional_array::<i32>("missing").unwrap().is_empty());
    assert!(g.optional_array::<u32>("a.durations").is_err());
    assert_eq!(g.get::<f32>("a.alpha").unwrap(), 0.97);

    assert_eq!(g.tensor("w", &[2, 1]).unwrap().to_f32().unwrap(), [1.5, -2.0]);
    assert!(g.tensor("w", &[1, 2]).is_err());

    let q = g.tensor("q", &[32]).unwrap();
    assert_eq!(q.ty, Type::Q8_0);
    let expected: Vec<f32> = (0..32).map(|i| (i - 16) as f32 * 0.5).collect();
    assert_eq!(q.to_f32().unwrap(), expected);

    let mut pieces = Vec::new();
    q.read(16, |offset, piece| pieces.push((offset, piece.len()))).unwrap();
    assert_eq!(pieces, [(0, 16), (16, 16), (32, 2)]);
  }

  /// A file with one tensor `name` of `dims` and ggml `id`, no metadata, and `data` as its body.
  fn one_tensor(name: &str, dims: &[usize], id: u32, data: &[u8]) -> Vec<u8> {
    let mut f = b"GGUF".to_vec();
    f.extend(3u32.to_le_bytes());
    f.extend(1u64.to_le_bytes());
    f.extend(0u64.to_le_bytes());
    string(&mut f, name);
    f.extend((dims.len() as u32).to_le_bytes());
    dims.iter().for_each(|&d| f.extend((d as u64).to_le_bytes()));
    f.extend(id.to_le_bytes());
    f.extend(0u64.to_le_bytes());
    f.resize(f.len().next_multiple_of(32), 0);
    f.extend(data);
    f
  }

  /// The 12 bytes of 6-bit scales and mins of a Q4_K/Q5_K block: scales 10, 20, 30, 40, 21, 6,
  /// 7, 8 and mins 11, 22, 33, 44, 35, 2, 1, 0, with the top bits of the last four in the
  /// high bits of the first four bytes.
  const SCALES: [u8; 12] =
    [10 | 1 << 6, 20, 30, 40, 11 | 2 << 6, 22, 33, 44, 0x35, 0x26, 0x17, 0x08];

  /// A Q4_K or Q5_K block of `len` bytes with scale `d`, min `dmin`, `SCALES` and zero quants.
  fn k_block(d: f32, dmin: f32, len: usize) -> Vec<u8> {
    let mut b = vec![0u8; len];
    b[..2].copy_from_slice(&f16::from_f32(d).to_le_bytes());
    b[2..4].copy_from_slice(&f16::from_f32(dmin).to_le_bytes());
    b[4..16].copy_from_slice(&SCALES);
    b
  }

  #[test]
  fn maps_ggml_types_and_sizes() {
    assert_eq!(Type::from_ggml(12), Some(Type::Q4_K));
    assert_eq!(Type::from_ggml(13), Some(Type::Q5_K));
    assert_eq!(Type::from_ggml(14), Some(Type::Q6_K));
    assert_eq!(Type::from_ggml(11), None);
    assert_eq!(Type::Q4_K.size(512), 2 * 144);
    assert_eq!(Type::Q5_K.size(256), 176);
    assert_eq!(Type::Q6_K.size(256 * 3), 3 * 210);
    assert_eq!(Type::Q8_0.size(64), 2 * 34);
    assert_eq!(Type::F16.size(64), 128);
  }

  #[test]
  fn rejects_partial_k_blocks() {
    let err = parse(one_tensor("t", &[200], 12, &[0; 144])).err().unwrap();
    assert!(err.to_string().contains("200 values"), "{err}");
    assert!(parse(one_tensor("t", &[256], 12, &[0; 144])).is_ok());
  }

  #[test]
  fn dequantizes_q4_k_scales_and_mins() {
    let mut block = k_block(0.5, 0.25, 144);
    block[16] = 0x21; // chunk 0: 1 in the low nibble, 2 in the high
    block[16 + 32] = 0x43; // chunk 1: 3 and 4
    block[16 + 64] = 0x05; // chunk 2: 5 and 0

    let g = Gguf::with_tensors(&[("t", &[256], Type::Q4_K, &block)]);
    let w = g.tensor("t", &[256]).unwrap().to_f32().unwrap();
    // d·s·q − dmin·m, with d = 0.5 and dmin = 0.25.
    assert_eq!(w[0], 0.5 * 10.0 * 1.0 - 0.25 * 11.0);
    assert_eq!(w[32], 0.5 * 20.0 * 2.0 - 0.25 * 22.0);
    assert_eq!(w[64], 0.5 * 30.0 * 3.0 - 0.25 * 33.0);
    assert_eq!(w[96], 0.5 * 40.0 * 4.0 - 0.25 * 44.0);
    // Sub-blocks 4 and 5 take their scales' top bits from the first four bytes: 5 | 1 << 4 = 21
    // and 3 | 2 << 4 = 35.
    assert_eq!(w[128], 0.5 * 21.0 * 5.0 - 0.25 * 35.0);
    assert_eq!(w[160], -0.25 * 2.0);
    // Sub-blocks 6 and 7 are zeroed in `qs`.
    assert_eq!(w[192], -0.25 * 1.0);
    assert_eq!(w[224], 0.0);
    // A zero quant is the min, not zero.
    assert_eq!(w[1], -0.25 * 11.0);
    assert_eq!(w[193], -0.25 * 1.0);
    assert_eq!(w[255], 0.0);
  }

  #[test]
  fn dequantizes_q5_k_fifth_bit() {
    let mut block = k_block(0.5, 0.25, 176);
    block[16] = 0b0000_0101; // the fifth bit of chunks 0 and 1's low nibbles
    block[17] = 0b0000_1010; // and of chunk 0's high nibbles
    block[48] = 0x21;
    block[48 + 32] = 0x43;

    let g = Gguf::with_tensors(&[("t", &[256], Type::Q5_K, &block)]);
    let w = g.tensor("t", &[256]).unwrap().to_f32().unwrap();
    assert_eq!(w[0], 0.5 * 10.0 * 17.0 - 0.25 * 11.0);
    assert_eq!(w[32], 0.5 * 20.0 * 2.0 - 0.25 * 22.0);
    assert_eq!(w[64], 0.5 * 30.0 * 19.0 - 0.25 * 33.0);
    assert_eq!(w[96], 0.5 * 40.0 * 4.0 - 0.25 * 44.0);
    assert_eq!(w[1], -0.25 * 11.0);
    assert_eq!(w[33], 0.5 * 20.0 * 16.0 - 0.25 * 22.0);
    assert_eq!(w[97], 0.5 * 40.0 * 16.0 - 0.25 * 44.0);
    // Chunk 2's bits are 4 and 5, clear in `qh[1]`.
    assert_eq!(w[161], -0.25 * 2.0);
  }

  /// A Q6_K block of `d = 0.5` and the scales 1 to 16, with zero quants.
  fn q6_k_block() -> Vec<u8> {
    let mut b = vec![0u8; 210];
    b[192..208].copy_from_slice(&(1u8..=16).collect::<Vec<_>>());
    b[208..].copy_from_slice(&f16::from_f32(0.5).to_le_bytes());
    b
  }

  #[test]
  fn dequantizes_q6_k_scales_and_offsets() {
    let mut block = q6_k_block();

    let g = Gguf::with_tensors(&[("t", &[256], Type::Q6_K, &block)]);
    let w = g.tensor("t", &[256]).unwrap().to_f32().unwrap();
    // A zero quant byte is 0 − 32, scaled by each of the 16 scales in turn.
    for l in 0..32 {
      let sc = if l < 16 { 1.0 } else { 2.0 };
      assert_eq!(w[l], 0.5 * sc * -32.0);
      assert_eq!(w[l + 32], 0.5 * (sc + 2.0) * -32.0);
      assert_eq!(w[l + 64], 0.5 * (sc + 4.0) * -32.0);
      assert_eq!(w[l + 96], 0.5 * (sc + 6.0) * -32.0);
    }
    assert_eq!(w[128], 0.5 * 9.0 * -32.0);
    assert_eq!(w[255], 0.5 * 16.0 * -32.0);

    // A negative scale (a sub-block of mostly negative weights) must stay negative.
    block[192] = 0xFF; // -1
    block[193] = 0x80; // -128
    let g = Gguf::with_tensors(&[("t", &[256], Type::Q6_K, &block)]);
    let w = g.tensor("t", &[256]).unwrap().to_f32().unwrap();
    assert_eq!(w[0], 0.5 * -1.0 * -32.0);
    assert_eq!(w[16], 0.5 * -128.0 * -32.0);
    assert_eq!(w[32], 0.5 * 3.0 * -32.0);

    // The high bits of the quants: qh byte 0 holds 0, 1, 2 and 3 in its pairs.
    let mut block = q6_k_block();
    block[128] = 0b1110_0100;
    block[0] = 0x0A;
    block[32] = 0x05;
    let g = Gguf::with_tensors(&[("t", &[256], Type::Q6_K, &block)]);
    let w = g.tensor("t", &[256]).unwrap().to_f32().unwrap();
    assert_eq!(w[0], 0.5 * 1.0 * (10.0 - 32.0));
    assert_eq!(w[32], 0.5 * 3.0 * (21.0 - 32.0));
    assert_eq!(w[64], 0.5 * 5.0 * (32.0 - 32.0));
    assert_eq!(w[96], 0.5 * 7.0 * (48.0 - 32.0));
  }

  /// Not part of the suite: dequantizes a real K-quant model against its Q8_0 twin.
  /// `WAVO_QUANT_DIR=/tmp/opencode/wavo-quant cargo test --lib -- --ignored crosschecks`
  #[test]
  #[ignore]
  fn crosschecks_real_models() {
    let dir = std::env::var("WAVO_QUANT_DIR").unwrap();
    let base = Gguf::open(&PathBuf::from(&dir).join("whisper-base.en-Q8_0.gguf")).unwrap();
    for q in ["Q4_K_M", "Q5_K_M", "Q6_K"] {
      let g = Gguf::open(&PathBuf::from(&dir).join(format!("whisper-base.en-{q}.gguf"))).unwrap();
      let (mut worst, mut sum, mut tensors) = ((0f64, String::new()), 0f64, 0usize);
      let mut names: Vec<_> = g.tensors.iter().collect();
      names.sort_by(|a, b| a.0.cmp(b.0));
      for (name, t) in names {
        if !matches!(t.ty, Type::Q4_K | Type::Q5_K | Type::Q6_K) {
          continue;
        }
        assert_eq!(base.tensors[name].dims, t.dims, "{name}");
        let w = g.tensor(name, &t.dims).unwrap().to_f32().unwrap();
        let r = base.tensor(name, &t.dims).unwrap().to_f32().unwrap();
        let n = r.len() as f64;
        let mean = r.iter().map(|v| *v as f64).sum::<f64>() / n;
        let sd = (r.iter().map(|v| (*v as f64 - mean).powi(2)).sum::<f64>() / n).sqrt();
        let e = w.iter().zip(&r).map(|(a, b)| (*a as f64 - *b as f64).powi(2)).sum::<f64>();
        let rms = (e / r.len() as f64).sqrt() / sd;
        sum += rms;
        tensors += 1;
        if rms > worst.0 {
          worst = (rms, name.clone());
        }
        assert!(rms < 0.25, "{q} {name}: {rms:.3}σ from Q8_0");
      }
      println!(
        "{q}: {tensors} tensors, rms {:.4}σ mean, {:.4}σ worst ({})",
        sum / tensors as f64,
        worst.0,
        worst.1
      );
    }
  }

  #[test]
  fn keeps_f16_f32_and_q8_0_paths() {
    let f32s: Vec<u8> = [1.5f32, -2.0].iter().flat_map(|v| v.to_le_bytes()).collect();
    let f16s: Vec<u8> =
      [f16::from_f32(1.5), f16::from_f32(-2.0)].iter().flat_map(|v| v.to_le_bytes()).collect();
    let mut q8 = f16::from_f32(0.5).to_le_bytes().to_vec();
    q8.extend((0..32).map(|i| (i as i8 - 16) as u8));
    let g = Gguf::with_tensors(&[
      ("a", &[2], Type::F32, &f32s),
      ("b", &[2], Type::F16, &f16s),
      ("c", &[32], Type::Q8_0, &q8),
    ]);
    assert_eq!(g.tensor("a", &[2]).unwrap().to_f32().unwrap(), [1.5, -2.0]);
    assert_eq!(g.tensor("b", &[2]).unwrap().to_f32().unwrap(), [1.5, -2.0]);
    let expected: Vec<f32> = (0..32).map(|i| (i - 16) as f32 * 0.5).collect();
    assert_eq!(g.tensor("c", &[32]).unwrap().to_f32().unwrap(), expected);
  }

  #[test]
  fn rejects_truncated_files() {
    let mut f = sample();
    f.truncate(f.len() - 1);
    assert!(parse(f).is_err());
  }
}
