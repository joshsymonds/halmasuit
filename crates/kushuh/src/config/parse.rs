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
//! source [`Span`]; nothing here panics on user input.
//!
//! Coverage so far: `region` definitions, `system`/`role` nodes, inline
//! geometry (`monitor` + `rect`) or a shared `region="…"` reference, and
//! the `sticky` / flex bindings. The `cycle` and `pattern` binding kinds
//! are recognised and rejected with a clear "not yet supported" error —
//! they land in a follow-up task.

use std::collections::HashMap;

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

impl Config {
    /// Parse a KDL source string into a validated [`Config`].
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] (with a source span where known) for
    /// malformed KDL, an unrecognised schema, or a config that violates
    /// the model's invariants (bad region, duplicate names, …).
    pub fn from_kdl(src: &str) -> Result<Self, ParseError> {
        parse(src)
    }
}

/// Parse a KDL source string into a validated [`Config`]. See
/// [`Config::from_kdl`].
///
/// # Errors
///
/// See [`Config::from_kdl`].
pub fn parse(src: &str) -> Result<Config, ParseError> {
    let doc: KdlDocument = src.parse().map_err(|e| kdl_syntax_error(&e))?;

    // Pass 1: collect top-level `region "name" { monitor; rect }` anchors.
    let mut regions: HashMap<String, (String, Region)> = HashMap::new();
    let mut systems: Vec<System> = Vec::new();

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
            "system" => systems.push(parse_system(node, &regions)?),
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

/// A named string property of `node`, if present.
fn string_prop(node: &KdlNode, key: &str) -> Option<String> {
    node.entries()
        .iter()
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

/// Parse a role's binding child node. Absence of any binding node is
/// [`Binding::Flex`].
fn parse_binding(role: &KdlNode) -> Result<Binding, ParseError> {
    if let Some(node) = child(role, "sticky") {
        let app_id = first_string_arg(node, "app id")?;
        let profile = string_prop(node, "profile");
        let app = AppRef::new(app_id, profile)
            .map_err(|e| ParseError::at(e.to_string(), span_of(node)))?;
        return Ok(Binding::Sticky(app));
    }
    for unsupported in ["cycle", "pattern"] {
        if let Some(node) = child(role, unsupported) {
            return Err(ParseError::at(
                format!("binding kind `{unsupported}` is not yet supported"),
                span_of(node),
            ));
        }
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
    fn garbage_does_not_panic() {
        // The point is that this returns Err rather than panicking.
        let _ = parse("]]] not kdl \u{0}\u{1} ===");
        let _ = parse("");
        let _ = parse("system");
    }
}
