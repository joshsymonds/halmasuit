//! greetd wire-protocol server.
//!
//! v2 implementation: state machine + JSON-over-Unix-socket server speaking
//! the greetd protocol so existing greeters (DankGreeter, regreet, tuigreet,
//! gtkgreet) connect to halmasuit unchanged.
