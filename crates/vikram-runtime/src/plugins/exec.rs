use std::path::Path;

pub trait PluginArgs {
    fn to_args(&self) -> Vec<String>;
}

impl PluginArgs for &str {
    fn to_args(&self) -> Vec<String> {
        if self.trim().is_empty() {
            Vec::new()
        } else {
            self.split_whitespace().map(str::to_owned).collect()
        }
    }
}

impl PluginArgs for String {
    fn to_args(&self) -> Vec<String> {
        self.as_str().to_args()
    }
}

impl<A> PluginArgs for &[A]
where
    A: AsRef<str>,
{
    fn to_args(&self) -> Vec<String> {
        self.iter().map(|s| s.as_ref().to_owned()).collect()
    }
}

impl<A> PluginArgs for Vec<A>
where
    A: AsRef<str>,
{
    fn to_args(&self) -> Vec<String> {
        self.iter().map(|s| s.as_ref().to_owned()).collect()
    }
}

impl<A, const N: usize> PluginArgs for [A; N]
where
    A: AsRef<str>,
{
    fn to_args(&self) -> Vec<String> {
        self.iter().map(|s| s.as_ref().to_owned()).collect()
    }
}

impl<A, const N: usize> PluginArgs for &[A; N]
where
    A: AsRef<str>,
{
    fn to_args(&self) -> Vec<String> {
        self.iter().map(|s| s.as_ref().to_owned()).collect()
    }
}

pub(crate) fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        if !path.is_file() {
            return false;
        }
        if cfg!(windows) {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{}", e.to_uppercase()))
                .unwrap_or_default();
            if ext.is_empty() {
                return true;
            }
            let pathext =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_owned());
            return pathext.split(';').any(|e| e.eq_ignore_ascii_case(&ext));
        }
        true
    }
}
