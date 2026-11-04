//! Recovery commands remain usable when only the current conversation is unavailable.

use super::active_reconnect::drain_history;
use super::*;
use pretty_assertions::assert_eq;
