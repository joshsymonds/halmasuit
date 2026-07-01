//! Hand-written KDL (v2) → config-model parser.
//!
//! Converts a KDL source string into the resolved [`Config`] model. The
//! source schema is **B+**: roles are written inline inside each `system`
//! by default, and an optional top-level `region "name" { … }` lets
//! geometry be named once and shared by reference. Whatever the source
//! shape, it resolves to systems that *own* their roles.
//!
//! This is hand-written over the official `kdl` crate (no derive-macro
//! KDL, no serde-KDL — the repo owns its parsing boundaries). Every
//! malformed or incoherent input is a returned [`ParseError`] carrying a
//! source [`Span`]; nothing here panics on user input. Adversarially deep
//! or oversized input is rejected by a pre-parse guard (`guard_input`)
//! before it can overflow the recursive `kdl` parser.
//!
//! Coverage: `region` definitions, `system`/`role` nodes, inline geometry
//! (`monitor` + `rect`) or a shared `region="…"` reference, and all four
//! binding kinds — `sticky "app" [profile="…"]`, `cycle { app … }`,
//! `pattern app="…" [title="…"]`, and flex (no binding node).

use std::collections::{HashMap, HashSet};

use kdl::{KdlDocument, KdlNode, KdlValue};
use thiserror::Error;

use super::{AppRef, Binding, Config, Region, Role, System};

/// A byte span (offset + length) into the KDL source, for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    offset: usize,
    len: usize,
}

impl Span {
    /// Byte offset of the span's start in the source.
    #[must_use]
    pub const fn offset(self) -> usize {
        self.offset
    }

    /// Length of the span in bytes.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len
    }

    /// Whether the span covers zero bytes.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// A KDL config parse or resolution failure, with the source span of the
/// offending construct where one is known.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct ParseError {
    message: String,
    span: Option<Span>,
}

impl ParseError {
    /// The human-readable message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The source span of the offending construct, if known.
    #[must_use]
    pub const fn span(&self) -> Option<Span> {
        self.span
    }

    fn at(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span: Some(span),
        }
    }

    fn spanless(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span: None,
        }
    }
}

/// Maximum accepted config size in bytes. A desktop-layout config is
/// tiny; anything past this is almost certainly hostile or corrupt. Guards
/// against handing a huge document to the recursive `kdl` parser.
const MAX_CONFIG_BYTES: usize = 256 * 1024;

/// Maximum accepted `{`-nesting depth. The `kdl` parser recurses per
/// nested block with no depth bound of its own — a deeply-nested document
/// would overflow the stack before our code runs. A real config nests only
/// a few levels (system → role → cycle → app), so this is far above any
/// legitimate input.
const MAX_NESTING_DEPTH: usize = 32;

/// Reject pathological input *before* the recursive `kdl` parse: oversized
/// documents, and excessive `{`-nesting that would otherwise overflow the
/// stack inside `kdl` (which a returned error cannot catch). Brace counting
/// skips the contents of `"…"` strings so a brace inside a title regex
/// doesn't inflate the depth; raw/multi-line strings are counted
/// conservatively — this is a safety cap, not a second parser.
fn guard_input(src: &str) -> Result<(), ParseError> {
    if src.len() > MAX_CONFIG_BYTES {
        return Err(ParseError::spanless(format!(
            "config is {} bytes, over the {MAX_CONFIG_BYTES}-byte limit",
            src.len()
        )));
    }
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in src.bytes().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' => {
                    depth += 1;
                    if depth > MAX_NESTING_DEPTH {
                        return Err(ParseError::at(
                            format!(
                                "config nesting exceeds the depth limit of {MAX_NESTING_DEPTH}"
                            ),
                            Span { offset, len: 1 },
                        ));
                    }
                }
                b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Parse a KDL source string into a validated [`Config`]. This is the
/// crate's single config entry point.
///
/// # Errors
///
/// Returns a [`ParseError`] (with a source span where known) for malformed
/// KDL, an unrecognised schema, or a config that violates the model's
/// invariants (bad region, duplicate names, …).
pub fn parse(src: &str) -> Result<Config, ParseError> {
    guard_input(src)?;
    let doc: KdlDocument = src.parse().map_err(|e| kdl_syntax_error(&e))?;

    // Pass 1: collect top-level `region "name" { monitor; rect }` anchors.
    let mut regions: HashMap<String, (String, Region)> = HashMap::new();
    let mut systems: Vec<System> = Vec::new();
    let mut seen_systems: HashSet<String> = HashSet::new();

    for node in doc.nodes() {
        match node.name().value() {
            "region" => {
                let (name, monitor, region) = parse_region_def(node)?;
                if regions.insert(name.clone(), (monitor, region)).is_some() {
                    return Err(ParseError::at(
                        format!("duplicate region: {name}"),
                        span_of(node),
                    ));
                }
            }
            "system" => {
                let system = parse_system(node, &regions)?;
                if !seen_systems.insert(system.name().to_owned()) {
                    return Err(ParseError::at(
                        format!("duplicate system: {}", system.name()),
                        span_of(node),
                    ));
                }
                systems.push(system);
            }
            other => {
                return Err(ParseError::at(
                    format!("unexpected top-level node `{other}` (expected `region` or `system`)"),
                    span_of(node),
                ));
            }
        }
    }

    Config::new(systems).map_err(|e| ParseError::spanless(e.to_string()))
}

/// Map a `kdl` parse failure to a [`ParseError`], keeping the first
/// diagnostic's span when one is available.
fn kdl_syntax_error(err: &kdl::KdlError) -> ParseError {
    err.diagnostics.first().map_or_else(
        || ParseError::spanless(format!("invalid KDL: {err}")),
        |d| {
            let message = d
                .message
                .clone()
                .unwrap_or_else(|| "invalid KDL".to_owned());
            ParseError::at(
                format!("invalid KDL: {message}"),
                Span {
                    offset: d.span.offset(),
                    len: d.span.len(),
                },
            )
        },
    )
}

fn span_of(node: &KdlNode) -> Span {
    let span = node.span();
    Span {
        offset: span.offset(),
        len: span.len(),
    }
}

/// First positional (un-named) entry of `node` as a string.
fn first_string_arg(node: &KdlNode, what: &str) -> Result<String, ParseError> {
    node.entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            ParseError::at(
                format!("`{}` needs a {what}", node.name().value()),
                span_of(node),
            )
        })
}

/// A named string property of `node`, if present. When a key is repeated,
/// the rightmost wins, per the KDL v2 spec (so we scan from the back).
fn string_prop(node: &KdlNode, key: &str) -> Option<String> {
    node.entries()
        .iter()
        .rev()
        .find(|e| e.name().map(kdl::KdlIdentifier::value) == Some(key))
        .and_then(|e| e.value().as_string())
        .map(ToOwned::to_owned)
}

/// Find a single child node of `parent` by name.
fn child<'a>(parent: &'a KdlNode, name: &str) -> Option<&'a KdlNode> {
    parent
        .children()
        .into_iter()
        .flat_map(KdlDocument::nodes)
        .find(|n| n.name().value() == name)
}

/// Parse a top-level `region "name" { monitor "M"; rect x y w h }`.
fn parse_region_def(node: &KdlNode) -> Result<(String, String, Region), ParseError> {
    let name = first_string_arg(node, "name")?;
    let monitor = parse_monitor(node)?;
    let region = parse_rect(node)?;
    Ok((name, monitor, region))
}

/// Read the `monitor "M"` child node's string value.
fn parse_monitor(parent: &KdlNode) -> Result<String, ParseError> {
    let node = child(parent, "monitor").ok_or_else(|| {
        ParseError::at(
            format!("`{}` needs a `monitor`", parent.name().value()),
            span_of(parent),
        )
    })?;
    first_string_arg(node, "monitor name")
}

/// Read the `rect x y w h` child node into a validated [`Region`].
fn parse_rect(parent: &KdlNode) -> Result<Region, ParseError> {
    let node = child(parent, "rect").ok_or_else(|| {
        ParseError::at(
            format!("`{}` needs a `rect x y w h`", parent.name().value()),
            span_of(parent),
        )
    })?;
    let args: Vec<&KdlValue> = node
        .entries()
        .iter()
        .filter(|e| e.name().is_none())
        .map(kdl::KdlEntry::value)
        .collect();
    if args.len() != 4 {
        return Err(ParseError::at(
            format!(
                "`rect` needs exactly 4 numbers (x y w h), got {}",
                args.len()
            ),
            span_of(node),
        ));
    }
    let mut coords = [0u8; 4];
    for (slot, value) in coords.iter_mut().zip(args) {
        *slot = coord(value, node)?;
    }
    let [x, y, w, h] = coords;
    Region::new(x, y, w, h).map_err(|e| ParseError::at(e.to_string(), span_of(node)))
}

/// A single `rect` coordinate: an integer in `0..=100`.
fn coord(value: &KdlValue, node: &KdlNode) -> Result<u8, ParseError> {
    let n = value
        .as_integer()
        .ok_or_else(|| ParseError::at("`rect` coordinates must be integers", span_of(node)))?;
    u8::try_from(n).ok().filter(|n| *n <= 100).ok_or_else(|| {
        ParseError::at(
            format!("`rect` coordinate {n} is out of 0..=100"),
            span_of(node),
        )
    })
}

/// Parse a `system "name" { role … }` node.
fn parse_system(
    node: &KdlNode,
    regions: &HashMap<String, (String, Region)>,
) -> Result<System, ParseError> {
    let name = first_string_arg(node, "name")?;
    let mut roles = Vec::new();
    for child in node.children().into_iter().flat_map(KdlDocument::nodes) {
        match child.name().value() {
            "role" => roles.push(parse_role(child, regions)?),
            other => {
                return Err(ParseError::at(
                    format!("unexpected node `{other}` in system `{name}` (expected `role`)"),
                    span_of(child),
                ));
            }
        }
    }
    System::new(name, roles).map_err(|e| ParseError::at(e.to_string(), span_of(node)))
}

/// Parse a `role "name" [region="ref"] { [monitor; rect] <binding> }` node.
fn parse_role(
    node: &KdlNode,
    regions: &HashMap<String, (String, Region)>,
) -> Result<Role, ParseError> {
    let name = first_string_arg(node, "name")?;
    let region_ref = string_prop(node, "region");
    let has_inline_geometry = child(node, "monitor").is_some() || child(node, "rect").is_some();

    let (monitor, region) = match region_ref {
        Some(reference) => {
            if has_inline_geometry {
                return Err(ParseError::at(
                    format!(
                        "role `{name}` has both a shared `region` and inline geometry — pick one"
                    ),
                    span_of(node),
                ));
            }
            let (monitor, region) = regions.get(&reference).ok_or_else(|| {
                ParseError::at(
                    format!("role `{name}` references unknown region `{reference}`"),
                    span_of(node),
                )
            })?;
            (monitor.clone(), *region)
        }
        None => (parse_monitor(node)?, parse_rect(node)?),
    };

    let binding = parse_binding(node)?;
    Role::new(name, monitor, region, binding)
        .map_err(|e| ParseError::at(e.to_string(), span_of(node)))
}

/// Parse an `app "id" [profile="…"]` node (the shape of a `sticky` node
/// and of each `cycle` candidate) into an [`AppRef`].
fn parse_app(node: &KdlNode) -> Result<AppRef, ParseError> {
    let app_id = first_string_arg(node, "app id")?;
    let profile = string_prop(node, "profile");
    AppRef::new(app_id, profile).map_err(|e| ParseError::at(e.to_string(), span_of(node)))
}

/// Parse a role's binding child node. A role has at most one binding node;
/// the absence of one is [`Binding::Flex`].
fn parse_binding(role: &KdlNode) -> Result<Binding, ParseError> {
    let present: Vec<&str> = ["sticky", "cycle", "pattern"]
        .into_iter()
        .filter(|kind| child(role, kind).is_some())
        .collect();
    if present.len() > 1 {
        return Err(ParseError::at(
            format!(
                "role has multiple bindings ({}) — a role has exactly one",
                present.join(", ")
            ),
            span_of(role),
        ));
    }

    if let Some(node) = child(role, "sticky") {
        return Ok(Binding::Sticky(parse_app(node)?));
    }
    if let Some(node) = child(role, "cycle") {
        let candidates = node
            .children()
            .into_iter()
            .flat_map(KdlDocument::nodes)
            .map(|app| {
                if app.name().value() == "app" {
                    parse_app(app)
                } else {
                    Err(ParseError::at(
                        format!(
                            "unexpected node `{}` in `cycle` (expected `app`)",
                            app.name().value()
                        ),
                        span_of(app),
                    ))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Binding::cycle(candidates)
            .map_err(|e| ParseError::at(e.to_string(), span_of(node)));
    }
    if let Some(node) = child(role, "pattern") {
        let app_id = string_prop(node, "app")
            .ok_or_else(|| ParseError::at("`pattern` needs an `app=\"…\"`", span_of(node)))?;
        let title = string_prop(node, "title");
        return Binding::pattern(app_id, title)
            .map_err(|e| ParseError::at(e.to_string(), span_of(node)));
    }
    Ok(Binding::Flex)
}

#[cfg(test)]
mod tests {
    use super::{ParseError, parse};
    use crate::config::{Binding, Region};

    fn parse_ok(src: &str) -> crate::config::Config {
        parse(src).expect("config should parse")
    }

    fn parse_err(src: &str) -> ParseError {
        parse(src).expect_err("config should fail to parse")
    }

    #[test]
    fn parses_minimal_inline_config() {
        let config = parse_ok(
            r#"
            system "code" {
                role "editor" {
                    monitor "DP-1"
                    rect 0 0 100 100
                    sticky "kitty"
                }
            }
            "#,
        );
        assert_eq!(config.systems().len(), 1);
        let system = config.system("code").expect("system code exists");
        assert_eq!(system.roles().len(), 1);
        let role = &system.roles()[0];
        assert_eq!(role.name(), "editor");
        assert_eq!(role.monitor(), "DP-1");
        assert_eq!(role.region(), Region::new(0, 0, 100, 100).unwrap());
        match role.binding() {
            Binding::Sticky(app) => {
                assert_eq!(app.app_id(), "kitty");
                assert_eq!(app.profile(), None);
            }
            other => panic!("expected sticky binding, got {other:?}"),
        }
    }

    #[test]
    fn parses_shared_region_reference_with_profile() {
        let config = parse_ok(
            r#"
            region "main-right" {
                monitor "DP-2"
                rect 60 0 40 100
            }
            system "personal" {
                role "browser" region="main-right" {
                    sticky "firefox" profile="personal"
                }
            }
            "#,
        );
        let role = &config.system("personal").unwrap().roles()[0];
        assert_eq!(role.monitor(), "DP-2");
        assert_eq!(role.region(), Region::new(60, 0, 40, 100).unwrap());
        match role.binding() {
            Binding::Sticky(app) => {
                assert_eq!(app.app_id(), "firefox");
                assert_eq!(app.profile(), Some("personal"));
            }
            other => panic!("expected sticky binding, got {other:?}"),
        }
    }

    #[test]
    fn role_without_binding_is_flex() {
        let config = parse_ok(
            r#"
            system "code" {
                role "scratch" {
                    monitor "DP-2"
                    rect 0 0 40 100
                }
            }
            "#,
        );
        let role = &config.system("code").unwrap().roles()[0];
        assert_eq!(role.binding(), &Binding::Flex);
    }

    #[test]
    fn duplicate_property_resolves_last_wins() {
        // KDL v2: when a node repeats a property key, the rightmost wins.
        let config = parse_ok(
            r#"
            system "s" {
                role "r" {
                    monitor "DP-1"
                    rect 0 0 100 100
                    sticky "firefox" profile="first" profile="second"
                }
            }
            "#,
        );
        match config.system("s").unwrap().roles()[0].binding() {
            Binding::Sticky(app) => assert_eq!(app.profile(), Some("second")),
            other => panic!("expected sticky binding, got {other:?}"),
        }
    }

    #[test]
    fn malformed_kdl_yields_spanned_error() {
        // Unclosed system block.
        let err = parse_err("system \"x\" {");
        assert!(err.span().is_some(), "syntax error should carry a span");
    }

    #[test]
    fn invalid_region_surfaces_model_error_with_span() {
        // rect with zero width → RegionError::ZeroWidth, wrapped with the
        // rect node's span.
        let err = parse_err(
            r#"
            system "code" {
                role "bad" {
                    monitor "DP-1"
                    rect 0 0 0 100
                    sticky "kitty"
                }
            }
            "#,
        );
        assert!(err.message().contains("width"), "got: {}", err.message());
        assert!(err.span().is_some(), "schema error should carry a span");
    }

    #[test]
    fn region_and_inline_geometry_is_an_error() {
        let err = parse_err(
            r#"
            region "r" { monitor "DP-1"; rect 0 0 100 100 }
            system "code" {
                role "x" region="r" {
                    monitor "DP-2"
                    rect 0 0 50 50
                    sticky "kitty"
                }
            }
            "#,
        );
        assert!(err.span().is_some());
    }

    #[test]
    fn unknown_region_reference_is_an_error() {
        let err = parse_err(
            r#"
            system "code" {
                role "x" region="nope" { sticky "kitty" }
            }
            "#,
        );
        assert!(err.message().contains("nope"), "got: {}", err.message());
        assert!(err.span().is_some());
    }

    #[test]
    fn duplicate_region_name_is_an_error() {
        let err = parse_err(
            r#"
            region "r" { monitor "DP-1"; rect 0 0 50 100 }
            region "r" { monitor "DP-2"; rect 0 0 60 100 }
            system "s" { role "x" region="r" { sticky "kitty" } }
            "#,
        );
        assert!(
            err.message().contains("duplicate region"),
            "got: {}",
            err.message()
        );
        assert!(err.span().is_some());
    }

    #[test]
    fn duplicate_system_name_is_a_spanned_error() {
        let err = parse_err(
            r#"
            system "code" { role "a" { monitor "DP-1"; rect 0 0 100 100 } }
            system "code" { role "b" { monitor "DP-1"; rect 0 0 100 100 } }
            "#,
        );
        assert!(
            err.message().contains("duplicate system"),
            "got: {}",
            err.message()
        );
        assert!(
            err.span().is_some(),
            "duplicate-system error must carry a span, like duplicate region/role"
        );
    }

    #[test]
    fn unexpected_top_level_node_is_an_error() {
        let err = parse_err(r#"widget "x" { }"#);
        assert!(
            err.message().contains("unexpected top-level node"),
            "got: {}",
            err.message()
        );
        assert!(err.span().is_some());
    }

    #[test]
    fn unexpected_node_inside_system_is_an_error() {
        let err = parse_err(
            r#"
            system "s" {
                widget "x" { }
            }
            "#,
        );
        assert!(
            err.message().contains("expected `role`"),
            "got: {}",
            err.message()
        );
        assert!(err.span().is_some());
    }

    #[test]
    fn role_missing_monitor_is_an_error() {
        let err = parse_err(
            r#"
            system "s" {
                role "x" { rect 0 0 100 100; sticky "kitty" }
            }
            "#,
        );
        assert!(err.message().contains("monitor"), "got: {}", err.message());
        assert!(err.span().is_some());
    }

    #[test]
    fn role_missing_rect_is_an_error() {
        let err = parse_err(
            r#"
            system "s" {
                role "x" { monitor "DP-1"; sticky "kitty" }
            }
            "#,
        );
        assert!(err.message().contains("rect"), "got: {}", err.message());
        assert!(err.span().is_some());
    }

    #[test]
    fn rect_with_wrong_arg_count_is_an_error() {
        let err = parse_err(
            r#"
            system "s" {
                role "x" { monitor "DP-1"; rect 0 0 100; sticky "kitty" }
            }
            "#,
        );
        assert!(
            err.message().contains("4 numbers"),
            "got: {}",
            err.message()
        );
        assert!(err.span().is_some());
    }

    #[test]
    fn rect_with_non_integer_coordinate_is_an_error() {
        let err = parse_err(
            r#"
            system "s" {
                role "x" { monitor "DP-1"; rect 0 0 50.5 100; sticky "kitty" }
            }
            "#,
        );
        assert!(err.message().contains("integers"), "got: {}", err.message());
        assert!(err.span().is_some());
    }

    #[test]
    fn rect_coordinate_over_100_is_an_error() {
        // A valid integer that is out of the 0..=100 percentage range —
        // distinct from a valid-int-but-bad-region (e.g. zero width).
        let err = parse_err(
            r#"
            system "s" {
                role "x" { monitor "DP-1"; rect 0 0 200 100; sticky "kitty" }
            }
            "#,
        );
        assert!(err.message().contains("0..=100"), "got: {}", err.message());
        assert!(err.span().is_some());
    }

    #[test]
    fn role_without_a_name_is_an_error() {
        let err = parse_err(
            r#"
            system "s" {
                role { monitor "DP-1"; rect 0 0 100 100 }
            }
            "#,
        );
        assert!(err.message().contains("name"), "got: {}", err.message());
        assert!(err.span().is_some());
    }

    #[test]
    fn duplicate_role_name_surfaces_through_parser_with_span() {
        let err = parse_err(
            r#"
            system "s" {
                role "dup" { monitor "DP-1"; rect 0 0 100 100 }
                role "dup" { monitor "DP-2"; rect 0 0 100 100 }
            }
            "#,
        );
        assert!(
            err.message().contains("duplicate role"),
            "got: {}",
            err.message()
        );
        assert!(
            err.span().is_some(),
            "parser must attach a span to the wrapped System error"
        );
    }

    #[test]
    fn error_span_locates_the_offending_node() {
        // Span correctness, not mere presence: the out-of-range rect's span
        // must cover the `rect …` text in the source, not byte 0.
        let src = r#"
            system "s" {
                role "x" { monitor "DP-1"; rect 0 0 200 100; sticky "kitty" }
            }
            "#;
        let err = parse_err(src);
        let span = err.span().expect("schema error carries a span");
        assert!(!span.is_empty(), "span should cover the rect node");
        let end = span.offset() + span.len();
        assert!(end <= src.len(), "span must lie within the source");
        assert_eq!(&src[span.offset()..end], "rect 0 0 200 100");
    }

    #[test]
    fn garbage_does_not_panic() {
        // The point is that this returns Err rather than panicking.
        let _ = parse("]]] not kdl \u{0}\u{1} ===");
        let _ = parse("");
        let _ = parse("system");
    }

    #[test]
    fn deeply_nested_input_is_rejected_not_crashed() {
        // Far past MAX_NESTING_DEPTH: the guard must reject this with an
        // error (not recurse into kdl and overflow the stack).
        let deep = "{".repeat(super::MAX_NESTING_DEPTH + 50);
        let err = parse_err(&deep);
        assert!(err.message().contains("depth"), "got: {}", err.message());
        assert!(err.span().is_some());
    }

    #[test]
    fn oversized_input_is_rejected() {
        let huge = "a".repeat(super::MAX_CONFIG_BYTES + 1);
        let err = parse_err(&huge);
        assert!(err.message().contains("limit"), "got: {}", err.message());
    }

    #[test]
    fn many_flat_systems_parse_within_limits() {
        // Large-ish but shallow (depth 3) and within the byte limit: the
        // guard must NOT reject legitimate flat input.
        use std::fmt::Write as _;
        let mut src = String::new();
        for i in 0..50 {
            writeln!(
                src,
                "system \"s{i}\" {{ role \"r\" {{ monitor \"DP-1\"; rect 0 0 100 100; sticky \"kitty\" }} }}"
            )
            .expect("writing to a String is infallible");
        }
        let config = parse_ok(&src);
        assert_eq!(config.systems().len(), 50);
    }

    #[test]
    fn cycle_binding_parses_with_per_candidate_profiles() {
        let config = parse_ok(
            r#"
            system "code" {
                role "companion" {
                    monitor "DP-2"
                    rect 0 0 40 100
                    cycle {
                        app "claude-desktop"
                        app "spotify" profile="muzak"
                    }
                }
            }
            "#,
        );
        let role = &config.system("code").unwrap().roles()[0];
        match role.binding() {
            Binding::Cycle(candidates) => {
                assert_eq!(candidates.len(), 2);
                assert_eq!(candidates[0].app_id(), "claude-desktop");
                assert_eq!(candidates[0].profile(), None);
                assert_eq!(candidates[1].app_id(), "spotify");
                assert_eq!(candidates[1].profile(), Some("muzak"));
            }
            other => panic!("expected cycle binding, got {other:?}"),
        }
    }

    #[test]
    fn cycle_with_one_candidate_is_an_error() {
        let err = parse_err(
            r#"
            system "code" {
                role "x" {
                    monitor "DP-1"
                    rect 0 0 100 100
                    cycle { app "spotify" }
                }
            }
            "#,
        );
        assert!(err.message().contains("two"), "got: {}", err.message());
        assert!(err.span().is_some());
    }

    #[test]
    fn cycle_rejects_a_non_app_child() {
        let err = parse_err(
            r#"
            system "code" {
                role "x" {
                    monitor "DP-1"
                    rect 0 0 100 100
                    cycle { sticky "spotify" }
                }
            }
            "#,
        );
        assert!(err.message().contains("app"), "got: {}", err.message());
        assert!(err.span().is_some());
    }

    #[test]
    fn pattern_binding_parses() {
        let config = parse_ok(
            r#"
            system "meeting" {
                role "video" {
                    monitor "DP-2"
                    rect 0 0 100 100
                    pattern app="Zoom" title="^Meeting$"
                }
            }
            "#,
        );
        let role = &config.system("meeting").unwrap().roles()[0];
        assert_eq!(
            role.binding(),
            &Binding::Pattern {
                app_id: "Zoom".to_owned(),
                title_regex: Some("^Meeting$".to_owned()),
            },
        );
    }

    #[test]
    fn pattern_without_title_parses() {
        let config = parse_ok(
            r#"
            system "s" {
                role "r" {
                    monitor "DP-1"
                    rect 0 0 100 100
                    pattern app="firefox"
                }
            }
            "#,
        );
        let role = &config.system("s").unwrap().roles()[0];
        assert_eq!(
            role.binding(),
            &Binding::Pattern {
                app_id: "firefox".to_owned(),
                title_regex: None,
            },
        );
    }

    #[test]
    fn pattern_without_app_is_an_error() {
        let err = parse_err(
            r#"
            system "meeting" {
                role "video" {
                    monitor "DP-2"
                    rect 0 0 100 100
                    pattern title="^Meeting$"
                }
            }
            "#,
        );
        assert!(err.message().contains("app"), "got: {}", err.message());
        assert!(err.span().is_some());
    }

    #[test]
    fn role_with_two_bindings_is_an_error() {
        let err = parse_err(
            r#"
            system "s" {
                role "r" {
                    monitor "DP-1"
                    rect 0 0 100 100
                    sticky "kitty"
                    cycle { app "a"; app "b" }
                }
            }
            "#,
        );
        assert!(
            err.message().contains("multiple bindings"),
            "got: {}",
            err.message()
        );
        assert!(err.span().is_some());
    }

    /// The three `ARCHITECTURE.md` example layouts (`code` / `meeting` /
    /// `reading`), encoded in the B+ schema, parse and validate as one
    /// config — exercising every binding kind, a profile, a shared
    /// `region` referenced across systems, and flex.
    #[test]
    fn example_configs_parse_and_validate() {
        let config = parse_ok(
            r#"
            // shared geometry, named once
            region "right" { monitor "DP-2"; rect 40 0 60 100 }
            region "left"  { monitor "DP-2"; rect 0 0 40 100 }

            system "code" {
                role "editor" {
                    monitor "DP-1"
                    rect 0 0 100 100
                    sticky "kitty"
                }
                role "browser" region="right" {
                    sticky "firefox" profile="code"
                }
                role "companion" region="left" {
                    cycle {
                        app "claude-desktop"
                        app "spotify"
                    }
                }
                role "scratch" {
                    monitor "DP-3"
                    rect 0 80 100 20
                }
            }

            system "meeting" {
                role "video" {
                    monitor "DP-2"
                    rect 0 0 100 100
                    pattern app="Zoom" title="^Meeting$"
                }
                role "chat" {
                    monitor "DP-3"
                    rect 0 0 100 50
                    sticky "slack"
                }
                role "notes" {
                    monitor "DP-3"
                    rect 0 50 100 50
                    sticky "obsidian"
                }
            }

            system "reading" {
                role "primary" {
                    monitor "DP-3"
                    rect 0 0 100 100
                    pattern app="firefox" title="(?i)reader"
                }
            }
            "#,
        );

        assert_eq!(config.systems().len(), 3);

        // code: shared region resolved, profile carried, cycle + flex present.
        let code = config.system("code").unwrap();
        assert_eq!(code.roles().len(), 4);
        let browser = &code.roles()[1];
        assert_eq!(browser.monitor(), "DP-2");
        assert_eq!(browser.region(), Region::new(40, 0, 60, 100).unwrap());
        match browser.binding() {
            Binding::Sticky(app) => assert_eq!(app.profile(), Some("code")),
            other => panic!("expected sticky firefox, got {other:?}"),
        }
        assert!(matches!(code.roles()[2].binding(), Binding::Cycle(c) if c.len() == 2));
        assert_eq!(code.roles()[3].binding(), &Binding::Flex);

        // meeting: the Zoom catch.
        let video = &config.system("meeting").unwrap().roles()[0];
        assert_eq!(
            video.binding(),
            &Binding::Pattern {
                app_id: "Zoom".to_owned(),
                title_regex: Some("^Meeting$".to_owned()),
            },
        );

        // reading: a title-pattern firefox.
        let primary = &config.system("reading").unwrap().roles()[0];
        assert!(
            matches!(primary.binding(), Binding::Pattern { app_id, .. } if app_id == "firefox")
        );
    }
}
