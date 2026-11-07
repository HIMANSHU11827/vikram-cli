//! Platform adapters. Each one owns one wire protocol and implements [`crate::platform::PlatformAdapter`].
//!
//! The shared pieces — HTTP, chunking, the delivery ledger, pairing and the registry — live one
//! level up. An adapter only translates between the platform's wire format and the gateway's
//! event/result types.

pub mod a2a;
pub mod bluebubbles;
pub mod buzz;
pub mod dingtalk;
pub mod dingtalk_stream;
pub mod discord;
pub mod email;
pub mod feishu;
pub mod google_chat;
pub mod irc;
pub mod line;
pub mod matrix;
pub mod mattermost;
pub mod ntfy;
pub mod photon;
pub mod qqbot;
pub mod raft;
pub mod signal;
pub mod simplex;
pub mod slack;
pub mod sms;
pub mod teams;
pub mod telegram;
pub mod wecom;
pub mod weixin;
pub mod whatsapp;
pub mod whatsapp_cloud;
pub mod yuanbao;
