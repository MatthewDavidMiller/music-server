//! One response tree, two encodings.
//!
//! Subsonic responses are XML by default and JSON with `f=json`; the JSON
//! form is a mechanical mapping of the XML one (attributes become members,
//! repeated elements become arrays, element text becomes `value`). Handlers
//! build a [`Node`] tree once and [`render`] produces either.

use std::fmt::Write as _;

use ms_common::json::write_string;

pub const API_VERSION: &str = "1.16.1";
pub const SERVER_TYPE: &str = "music-server";

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Str(String),
    Int(i64),
    Bool(bool),
    Float(f64),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Child {
    One(Node),
    /// Repeated elements of one name; always an array in JSON, even with
    /// one or no members.
    List(&'static str, Vec<Node>),
    /// Repeated scalar elements (`<folder>1</folder>`), an array of scalars
    /// in JSON.
    Values(&'static str, Vec<Val>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub name: &'static str,
    pub attrs: Vec<(&'static str, Val)>,
    pub children: Vec<Child>,
    pub text: Option<String>,
}

impl Node {
    pub fn new(name: &'static str) -> Self {
        Self { name, attrs: Vec::new(), children: Vec::new(), text: None }
    }

    #[must_use]
    pub fn s(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.attrs.push((key, Val::Str(value.into())));
        self
    }

    #[must_use]
    pub fn i(mut self, key: &'static str, value: impl Into<i64>) -> Self {
        self.attrs.push((key, Val::Int(value.into())));
        self
    }

    #[must_use]
    pub fn u(self, key: &'static str, value: u64) -> Self {
        self.i(key, i64::try_from(value).unwrap_or(i64::MAX))
    }

    #[must_use]
    pub fn b(mut self, key: &'static str, value: bool) -> Self {
        self.attrs.push((key, Val::Bool(value)));
        self
    }

    #[must_use]
    pub fn f(mut self, key: &'static str, value: f64) -> Self {
        self.attrs.push((key, Val::Float(value)));
        self
    }

    /// A string attribute only when the value is non-empty.
    #[must_use]
    pub fn opt_s(self, key: &'static str, value: Option<&str>) -> Self {
        match value {
            Some(v) if !v.is_empty() => self.s(key, v),
            _ => self,
        }
    }

    /// An integer attribute only when the value is non-zero.
    #[must_use]
    pub fn nz(self, key: &'static str, value: u64) -> Self {
        if value == 0 { self } else { self.u(key, value) }
    }

    #[must_use]
    pub fn child(mut self, node: Node) -> Self {
        self.children.push(Child::One(node));
        self
    }

    #[must_use]
    pub fn list(mut self, name: &'static str, nodes: Vec<Node>) -> Self {
        self.children.push(Child::List(name, nodes));
        self
    }

    #[must_use]
    pub fn values(mut self, name: &'static str, values: Vec<Val>) -> Self {
        self.children.push(Child::Values(name, values));
        self
    }

    #[must_use]
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Xml,
    Json,
}

impl Format {
    pub fn content_type(self) -> &'static str {
        match self {
            Format::Xml => "text/xml; charset=utf-8",
            Format::Json => "application/json",
        }
    }
}

/// Whether `c` may appear in an XML 1.0 document.
fn xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

fn xml_escape(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if xml_char(c) => out.push(c),
            _ => {}
        }
    }
}

fn scalar(v: &Val) -> String {
    match v {
        Val::Str(s) => s.clone(),
        Val::Int(i) => i.to_string(),
        Val::Bool(b) => b.to_string(),
        Val::Float(f) => float(*f),
    }
}

fn float(f: f64) -> String {
    if f.is_finite() { format!("{f}") } else { "0".to_owned() }
}

fn xml_node(out: &mut String, n: &Node) {
    out.push('<');
    out.push_str(n.name);
    for (k, v) in &n.attrs {
        let _ = write!(out, " {k}=\"");
        xml_escape(out, &scalar(v));
        out.push('"');
    }
    if n.children.is_empty() && n.text.is_none() {
        out.push_str("/>");
        return;
    }
    out.push('>');
    if let Some(t) = &n.text {
        xml_escape(out, t);
    }
    for c in &n.children {
        match c {
            Child::One(node) => xml_node(out, node),
            Child::List(_, nodes) => nodes.iter().for_each(|node| xml_node(out, node)),
            Child::Values(name, values) => {
                for v in values {
                    let _ = write!(out, "<{name}>");
                    xml_escape(out, &scalar(v));
                    let _ = write!(out, "</{name}>");
                }
            }
        }
    }
    let _ = write!(out, "</{}>", n.name);
}

fn json_val(out: &mut String, v: &Val) {
    match v {
        Val::Str(s) => write_string(out, s),
        Val::Int(i) => out.push_str(&i.to_string()),
        Val::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Val::Float(f) => out.push_str(&float(*f)),
    }
}

/// The members of `n` (no braces).
fn json_members(out: &mut String, n: &Node, mut first: bool) {
    let mut sep = |out: &mut String| {
        if !first {
            out.push(',');
        }
        first = false;
    };
    for (k, v) in &n.attrs {
        sep(out);
        write_string(out, k);
        out.push(':');
        json_val(out, v);
    }
    if let Some(t) = &n.text {
        sep(out);
        out.push_str("\"value\":");
        write_string(out, t);
    }
    for c in &n.children {
        sep(out);
        match c {
            Child::One(node) => {
                write_string(out, node.name);
                out.push(':');
                json_object(out, node);
            }
            Child::List(name, nodes) => {
                write_string(out, name);
                out.push_str(":[");
                for (i, node) in nodes.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    json_object(out, node);
                }
                out.push(']');
            }
            Child::Values(name, values) => {
                write_string(out, name);
                out.push_str(":[");
                for (i, v) in values.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    json_val(out, v);
                }
                out.push(']');
            }
        }
    }
}

fn json_object(out: &mut String, n: &Node) {
    out.push('{');
    json_members(out, n, true);
    out.push('}');
}

/// The envelope every response shares.
fn envelope(ok: bool) -> Node {
    Node::new("subsonic-response")
        .s("status", if ok { "ok" } else { "failed" })
        .s("version", API_VERSION)
        .s("type", SERVER_TYPE)
        .s("serverVersion", ms_common::VERSION)
        .b("openSubsonic", true)
}

/// A complete response document with `payload` (if any) inside.
pub fn render(format: Format, payload: Option<Node>, ok: bool) -> String {
    let mut root = envelope(ok);
    match payload {
        // A nameless payload's children go straight into the envelope (the
        // OpenSubsonic extension list is a bare array there).
        Some(p) if p.name.is_empty() => root.children.extend(p.children),
        Some(p) => root = root.child(p),
        None => {}
    }
    let mut out = String::with_capacity(512);
    match format {
        Format::Xml => {
            out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
            // The namespace goes first, as the reference server writes it.
            out.push_str("<subsonic-response xmlns=\"http://subsonic.org/restapi\"");
            for (k, v) in &root.attrs {
                let _ = write!(out, " {k}=\"");
                xml_escape(&mut out, &scalar(v));
                out.push('"');
            }
            if root.children.is_empty() {
                out.push_str("/>");
            } else {
                out.push('>');
                for c in &root.children {
                    match c {
                        Child::One(n) => xml_node(&mut out, n),
                        Child::List(_, nodes) => nodes.iter().for_each(|n| xml_node(&mut out, n)),
                        Child::Values(..) => {}
                    }
                }
                out.push_str("</subsonic-response>");
            }
        }
        Format::Json => {
            out.push_str("{\"subsonic-response\":");
            json_object(&mut out, &root);
            out.push('}');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head() -> String {
        format!(
            "status=\"ok\" version=\"{API_VERSION}\" type=\"music-server\" serverVersion=\"{}\" openSubsonic=\"true\"",
            ms_common::VERSION
        )
    }

    #[test]
    fn ping() {
        assert_eq!(
            render(Format::Xml, None, true),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<subsonic-response xmlns=\"http://subsonic.org/restapi\" {}/>",
                head()
            )
        );
        assert_eq!(
            render(Format::Json, None, true),
            format!(
                "{{\"subsonic-response\":{{\"status\":\"ok\",\"version\":\"{API_VERSION}\",\"type\":\"music-server\",\"serverVersion\":\"{}\",\"openSubsonic\":true}}}}",
                ms_common::VERSION
            )
        );
    }

    #[test]
    fn nested_lists_values_and_text() {
        let payload = Node::new("x")
            .list("song", vec![Node::new("song").s("id", "a<\"&'>").i("n", 3)])
            .list("empty", vec![])
            .values("folder", vec![Val::Int(1)])
            .child(Node::new("genre").i("songCount", 2).text("R&B"));
        let xml = render(Format::Xml, Some(payload.clone()), true);
        assert!(xml.ends_with(
            "><x><song id=\"a&lt;&quot;&amp;&apos;&gt;\" n=\"3\"/><folder>1</folder><genre songCount=\"2\">R&amp;B</genre></x></subsonic-response>"
        ));
        let json = render(Format::Json, Some(payload), true);
        assert!(json.ends_with(
            ",\"x\":{\"song\":[{\"id\":\"a<\\\"&'>\",\"n\":3}],\"empty\":[],\"folder\":[1],\"genre\":{\"songCount\":2,\"value\":\"R&B\"}}}}"
        ), "{json}");
        let parsed = ms_common::json::parse(json.as_bytes()).unwrap();
        assert!(parsed.get("subsonic-response").is_some());
    }

    #[test]
    fn xml_drops_forbidden_characters() {
        let xml = render(Format::Xml, Some(Node::new("x").s("t", "a\u{1}b\u{fffe}c\td")), true);
        assert!(xml.contains("t=\"abc\td\""));
    }

    #[test]
    fn failed_envelope() {
        let err = Node::new("error").i("code", 40).s("message", "Wrong username or password");
        let xml = render(Format::Xml, Some(err), false);
        assert!(xml.contains("status=\"failed\""));
        assert!(xml.contains("<error code=\"40\" message=\"Wrong username or password\"/>"));
    }
}
