//! The player's Foverse account, for anything in Floptle that needs one.
//!
//! This was `floptle-hub::auth` until the engine needed it too. The Hub is the
//! obvious place to sign in from, but it is not the only one and it is not
//! always installed: a game exported from Floptle runs on a machine that may
//! never have seen the Hub, and it still wants to know who is playing.
//!
//! The reason this is Rust and not Lua is one line long: the provider mandates
//! **PKCE S256**, and a Lua script has no SHA-256. Rather than add one so every
//! game could re-run an auth flow badly, the flow lives here — once, correctly —
//! and a script asks for a *player*, never a token.
//!
//! Three pieces:
//!
//! * [`auth`] — the device flow itself (RFC 8628 + PKCE), the [`auth::Session`]
//!   it produces, and the OS-keyring store it persists to. Unchanged from the
//!   Hub's shipped implementation apart from being reusable.
//! * [`Account`] — a non-blocking facade over that flow. Sign-in runs on a
//!   worker thread and the caller reads a [`Phase`] each frame, because a game
//!   cannot block for the thirty seconds a person takes to approve in a browser.
//! * [`cloud`] — authorized calls to the Floptle Cloud API, with the bearer
//!   attached here and the token never handed out.
//!
//! **The Hub and a game share one session.** Same keyring entry, deliberately:
//! signing in to the Hub signs you in to the games you launch from it, and
//! signing in from a game means the Hub already knows you next time. One
//! account, one sign-in, however you got here.

pub mod auth;
pub mod cloud;

mod account;

pub use account::{Account, Phase};
pub use auth::{Entitlements, MemoryStore, Provider, Session, TokenStore, UserInfo};
pub use cloud::{CloudReply, DEFAULT_BASE};
