// Copyright 2026 Jakub Hlavnicka
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//! Just enough Turtle to read an LV2 bundle.
//!
//! An LV2 plugin describes itself in RDF — which binary to load, which ports it
//! has, what features it needs — written as Turtle. The usual way to read it is
//! lilv (on serd and sord), which would make every build of this host carry
//! three C libraries and every Windows and macOS build cross-compile them. The
//! subset LV2 bundles actually use is small: prefixes and a base, IRIs and
//! prefixed names, `a`, `;` and `,` lists, blank-node property lists, string,
//! number and boolean literals, and the odd collection. That is what this
//! parses, into a flat list of triples that [`Graph`] answers questions about.

use std::collections::HashMap;

use anyhow::{bail, Result};

pub const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// One RDF term.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Iri(String),
    /// A blank node, by a label unique within the graph.
    Blank(String),
    Literal {
        value: String,
        datatype: Option<String>,
    },
}

impl Node {
    pub fn iri(&self) -> Option<&str> {
        match self {
            Node::Iri(s) => Some(s),
            _ => None,
        }
    }

    /// The literal's text, or an IRI's.
    pub fn text(&self) -> Option<&str> {
        match self {
            Node::Literal { value, .. } => Some(value),
            Node::Iri(s) => Some(s),
            Node::Blank(_) => None,
        }
    }

    /// The literal as a number, as `lv2:index` and `lv2:default` are written.
    pub fn number(&self) -> Option<f64> {
        match self {
            Node::Literal { value, .. } => match value.as_str() {
                "true" => Some(1.0),
                "false" => Some(0.0),
                v => v.trim().parse().ok(),
            },
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Triple {
    pub subject: Node,
    pub predicate: String,
    pub object: Node,
}

/// Every triple read from a bundle's files.
#[derive(Default, Debug)]
pub struct Graph {
    pub triples: Vec<Triple>,
    /// How many blank nodes have been minted, so labels from different files
    /// (each of which may say `_:b0`) never collide.
    blanks: usize,
}

impl Graph {
    /// Parse `text` into the graph, resolving relative IRIs against `base`.
    pub fn parse(&mut self, text: &str, base: &str) -> Result<()> {
        let mut parser = Parser {
            src: text.as_bytes(),
            pos: 0,
            base: base.to_string(),
            prefixes: HashMap::new(),
            graph: self,
            labels: HashMap::new(),
        };
        parser.document()
    }

    /// Objects of `(subject, predicate, ?)`.
    pub fn objects<'g: 'q, 'q>(
        &'g self,
        subject: &'q Node,
        predicate: &'q str,
    ) -> impl Iterator<Item = &'g Node> + 'q {
        self.triples
            .iter()
            .filter(move |t| t.subject == *subject && t.predicate == predicate)
            .map(|t| &t.object)
    }

    /// The first object of `(subject, predicate, ?)`.
    pub fn object<'g>(&'g self, subject: &Node, predicate: &str) -> Option<&'g Node> {
        self.objects(subject, predicate).next()
    }

    /// Subjects of `(?, predicate, object)`.
    pub fn subjects<'g: 'q, 'q>(
        &'g self,
        predicate: &'q str,
        object: &'q Node,
    ) -> impl Iterator<Item = &'g Node> + 'q {
        self.triples
            .iter()
            .filter(move |t| t.predicate == predicate && t.object == *object)
            .map(|t| &t.subject)
    }

    /// Whether `(subject, rdf:type, class)` is in the graph.
    pub fn is_a(&self, subject: &Node, class: &str) -> bool {
        self.objects(subject, RDF_TYPE).any(|o| o.iri() == Some(class))
    }

    fn fresh_blank(&mut self) -> Node {
        self.blanks += 1;
        Node::Blank(format!("b{}", self.blanks))
    }
}

struct Parser<'a, 'g> {
    src: &'a [u8],
    pos: usize,
    base: String,
    prefixes: HashMap<String, String>,
    graph: &'g mut Graph,
    /// `_:label` → the blank node minted for it in this document.
    labels: HashMap<String, Node>,
}

impl Parser<'_, '_> {
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn at(&self, s: &str) -> bool {
        self.src[self.pos..].starts_with(s.as_bytes())
    }

    fn error<T>(&self, what: &str) -> Result<T> {
        let line = self.src[..self.pos.min(self.src.len())]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            + 1;
        bail!("Turtle syntax error on line {line}: {what}")
    }

    /// Skip whitespace and `#` comments.
    fn ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.pos += 1;
            } else if c == b'#' {
                while let Some(c) = self.peek() {
                    self.pos += 1;
                    if c == b'\n' {
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, c: u8) -> Result<()> {
        self.ws();
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            self.error(&format!("expected '{}'", c as char))
        }
    }

    fn document(&mut self) -> Result<()> {
        loop {
            self.ws();
            if self.peek().is_none() {
                return Ok(());
            }
            if self.at("@prefix") || self.at_keyword("PREFIX") {
                let sparql = !self.at("@");
                self.pos += if sparql { 6 } else { 7 };
                self.ws();
                let start = self.pos;
                while self.peek().is_some_and(|c| c != b':') {
                    self.pos += 1;
                }
                let name = String::from_utf8_lossy(&self.src[start..self.pos]).trim().to_string();
                self.pos += 1;
                self.ws();
                let iri = self.iriref()?;
                self.prefixes.insert(name, iri);
                if !sparql {
                    self.expect(b'.')?;
                }
            } else if self.at("@base") || self.at_keyword("BASE") {
                let sparql = !self.at("@");
                self.pos += 5;
                self.ws();
                self.base = self.iriref()?;
                if !sparql {
                    self.expect(b'.')?;
                }
            } else {
                self.statement()?;
            }
        }
    }

    /// A SPARQL-style directive keyword, matched case-insensitively and only
    /// as a whole word.
    fn at_keyword(&self, kw: &str) -> bool {
        let end = self.pos + kw.len();
        end <= self.src.len()
            && self.src[self.pos..end].eq_ignore_ascii_case(kw.as_bytes())
            && self.src.get(end).is_some_and(|c| c.is_ascii_whitespace())
    }

    fn statement(&mut self) -> Result<()> {
        self.ws();
        let subject = if self.peek() == Some(b'[') {
            // `[ ... ] .` on its own, or `[ ... ] p o .`
            let node = self.blank_property_list()?;
            self.ws();
            if self.peek() == Some(b'.') {
                self.pos += 1;
                return Ok(());
            }
            node
        } else {
            self.subject()?
        };
        self.predicate_object_list(&subject)?;
        self.expect(b'.')
    }

    fn subject(&mut self) -> Result<Node> {
        self.ws();
        match self.peek() {
            Some(b'(') => self.collection(),
            _ => self.term(),
        }
    }

    fn predicate_object_list(&mut self, subject: &Node) -> Result<()> {
        loop {
            self.ws();
            // A trailing `;` before `.` or `]` is allowed.
            if matches!(self.peek(), Some(b'.') | Some(b']') | None) {
                return Ok(());
            }
            let predicate = self.verb()?;
            loop {
                let object = self.object()?;
                self.graph.triples.push(Triple {
                    subject: subject.clone(),
                    predicate: predicate.clone(),
                    object,
                });
                self.ws();
                if self.peek() == Some(b',') {
                    self.pos += 1;
                    continue;
                }
                break;
            }
            self.ws();
            if self.peek() == Some(b';') {
                while self.peek() == Some(b';') {
                    self.pos += 1;
                    self.ws();
                }
                continue;
            }
            return Ok(());
        }
    }

    fn verb(&mut self) -> Result<String> {
        self.ws();
        if self.peek() == Some(b'a')
            && self
                .src
                .get(self.pos + 1)
                .is_some_and(|c| c.is_ascii_whitespace() || *c == b'<' || *c == b'[')
        {
            self.pos += 1;
            return Ok(RDF_TYPE.to_string());
        }
        match self.term()? {
            Node::Iri(iri) => Ok(iri),
            _ => self.error("a predicate must be an IRI"),
        }
    }

    fn object(&mut self) -> Result<Node> {
        self.ws();
        match self.peek() {
            Some(b'[') => self.blank_property_list(),
            Some(b'(') => self.collection(),
            Some(b'"') | Some(b'\'') => self.literal(),
            Some(c) if c.is_ascii_digit() || c == b'+' || c == b'-' || c == b'.' => self.number(),
            _ => {
                if self.word_is("true") || self.word_is("false") {
                    let value = if self.word_is("true") { "true" } else { "false" };
                    self.pos += value.len();
                    return Ok(Node::Literal {
                        value: value.to_string(),
                        datatype: Some("http://www.w3.org/2001/XMLSchema#boolean".to_string()),
                    });
                }
                self.term()
            }
        }
    }

    fn word_is(&self, w: &str) -> bool {
        self.at(w)
            && !self
                .src
                .get(self.pos + w.len())
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b':' || *c == b'_')
    }

    /// `[ p o ; ... ]` — a fresh blank node and its properties.
    fn blank_property_list(&mut self) -> Result<Node> {
        self.expect(b'[')?;
        let node = self.graph.fresh_blank();
        self.predicate_object_list(&node)?;
        self.expect(b']')?;
        Ok(node)
    }

    /// `( a b c )` — an `rdf:first`/`rdf:rest` chain.
    fn collection(&mut self) -> Result<Node> {
        self.expect(b'(')?;
        let nil = Node::Iri(format!("{RDF}nil"));
        let mut items = Vec::new();
        loop {
            self.ws();
            if self.peek() == Some(b')') {
                self.pos += 1;
                break;
            }
            if self.peek().is_none() {
                return self.error("unterminated collection");
            }
            items.push(self.object()?);
        }
        let mut head = nil;
        for item in items.into_iter().rev() {
            let cell = self.graph.fresh_blank();
            self.graph.triples.push(Triple {
                subject: cell.clone(),
                predicate: format!("{RDF}first"),
                object: item,
            });
            self.graph.triples.push(Triple {
                subject: cell.clone(),
                predicate: format!("{RDF}rest"),
                object: head,
            });
            head = cell;
        }
        Ok(head)
    }

    /// An IRI, prefixed name or blank-node label.
    fn term(&mut self) -> Result<Node> {
        self.ws();
        match self.peek() {
            Some(b'<') => Ok(Node::Iri(self.iriref()?)),
            Some(b'_') if self.src.get(self.pos + 1) == Some(&b':') => {
                self.pos += 2;
                let label = self.name_chars();
                if let Some(node) = self.labels.get(&label) {
                    return Ok(node.clone());
                }
                let node = self.graph.fresh_blank();
                self.labels.insert(label, node.clone());
                Ok(node)
            }
            Some(_) => {
                let start = self.pos;
                let prefix = self.name_chars();
                if self.peek() != Some(b':') {
                    self.pos = start;
                    return self.error("expected an IRI or a prefixed name");
                }
                self.pos += 1;
                let local = self.name_chars();
                let Some(ns) = self.prefixes.get(&prefix) else {
                    return self.error(&format!("undeclared prefix '{prefix}:'"));
                };
                Ok(Node::Iri(format!("{ns}{local}")))
            }
            None => self.error("unexpected end of file"),
        }
    }

    /// The characters of a prefix or local name. A trailing `.` belongs to the
    /// statement, not the name.
    fn name_chars(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.' || c == b'%' || c >= 0x80 {
                self.pos += 1;
            } else if c == b'\\' && self.pos + 1 < self.src.len() {
                self.pos += 2;
            } else {
                break;
            }
        }
        while self.pos > start && self.src[self.pos - 1] == b'.' {
            self.pos -= 1;
        }
        String::from_utf8_lossy(&self.src[start..self.pos]).replace('\\', "")
    }

    /// `<...>`, resolved against the base.
    fn iriref(&mut self) -> Result<String> {
        self.expect(b'<')?;
        let start = self.pos;
        while self.peek().is_some_and(|c| c != b'>') {
            self.pos += 1;
        }
        if self.peek().is_none() {
            return self.error("unterminated IRI");
        }
        let raw = String::from_utf8_lossy(&self.src[start..self.pos]).into_owned();
        self.pos += 1;
        Ok(resolve(&self.base, &raw))
    }

    fn literal(&mut self) -> Result<Node> {
        let quote = self.peek().unwrap();
        let long = self.src[self.pos..].starts_with(&[quote, quote, quote]);
        self.pos += if long { 3 } else { 1 };
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.peek() else {
                return self.error("unterminated string");
            };
            if long {
                if self.src[self.pos..].starts_with(&[quote, quote, quote]) {
                    self.pos += 3;
                    break;
                }
            } else if c == quote {
                self.pos += 1;
                break;
            } else if c == b'\n' {
                return self.error("newline in a short string");
            }
            if c == b'\\' {
                self.pos += 1;
                let e = self.peek().unwrap_or(b'\\');
                self.pos += 1;
                match e {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'u' | b'U' => {
                        let n = if e == b'u' { 4 } else { 8 };
                        let hex = String::from_utf8_lossy(
                            &self.src[self.pos..(self.pos + n).min(self.src.len())],
                        )
                        .into_owned();
                        self.pos += n;
                        if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                    }
                    other => out.push(other),
                }
                continue;
            }
            out.push(c);
            self.pos += 1;
        }
        let value = String::from_utf8_lossy(&out).into_owned();
        let mut datatype = None;
        if self.peek() == Some(b'@') {
            self.pos += 1;
            while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'-') {
                self.pos += 1;
            }
        } else if self.at("^^") {
            self.pos += 2;
            datatype = self.term()?.iri().map(str::to_string);
        }
        Ok(Node::Literal { value, datatype })
    }

    fn number(&mut self) -> Result<Node> {
        let start = self.pos;
        if matches!(self.peek(), Some(b'+') | Some(b'-')) {
            self.pos += 1;
        }
        while let Some(c) = self.peek() {
            let exponent_sign = (c == b'+' || c == b'-')
                && matches!(self.src.get(self.pos - 1), Some(b'e') | Some(b'E'));
            if c.is_ascii_digit() || c == b'e' || c == b'E' || exponent_sign {
                self.pos += 1;
            } else if c == b'.' && self.src.get(self.pos + 1).is_some_and(u8::is_ascii_digit) {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return self.error("expected a number");
        }
        Ok(Node::Literal {
            value: String::from_utf8_lossy(&self.src[start..self.pos]).into_owned(),
            datatype: None,
        })
    }
}

/// Resolve `iri` against `base`. Only what bundles use: an absolute IRI stays
/// as it is, a fragment or a relative file name is appended to the base's
/// directory.
fn resolve(base: &str, iri: &str) -> String {
    if iri.contains("://") || iri.starts_with("urn:") || iri.starts_with("file:") {
        return iri.to_string();
    }
    if iri.is_empty() {
        return base.to_string();
    }
    if let Some(fragment) = iri.strip_prefix('#') {
        let doc = base.split('#').next().unwrap_or(base);
        return format!("{doc}#{fragment}");
    }
    let dir = match base.rfind('/') {
        Some(i) => &base[..=i],
        None => base,
    };
    format!("{dir}{iri}")
}

/// Undo `%xx` escapes, as a file name in a `file://` IRI has them.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LV2: &str = "http://lv2plug.in/ns/lv2core#";

    #[test]
    fn a_plugin_description_reads_as_triples() {
        let text = r#"
@prefix lv2:  <http://lv2plug.in/ns/lv2core#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
# a comment
<http://example.org/kars>
    a lv2:InstrumentPlugin, lv2:Plugin ;
    lv2:binary <Kars_dsp.so> ;
    lv2:port [
        a lv2:InputPort, lv2:ControlPort ;
        lv2:index 2 ;
        lv2:symbol "sustain" ;
        lv2:default 0.00999999977648 ;
        lv2:minimum -1.5e1 ;
        lv2:maximum 1 ;
    ] , [
        a lv2:OutputPort ;
        lv2:index 3 ;
    ] ;
    rdfs:comment """multi
line""" ;
    lv2:microVersion 0 .
"#;
        let mut g = Graph::default();
        g.parse(text, "file:///bundles/Kars.lv2/manifest.ttl").unwrap();
        let plugin = Node::Iri("http://example.org/kars".into());
        assert!(g.is_a(&plugin, &format!("{LV2}Plugin")));
        assert_eq!(
            g.object(&plugin, &format!("{LV2}binary")).and_then(Node::iri),
            Some("file:///bundles/Kars.lv2/Kars_dsp.so")
        );
        let ports: Vec<&Node> = g.objects(&plugin, &format!("{LV2}port")).collect();
        assert_eq!(ports.len(), 2);
        let idx = g.object(ports[0], &format!("{LV2}index")).and_then(Node::number);
        assert_eq!(idx, Some(2.0));
        let min = g.object(ports[0], &format!("{LV2}minimum")).and_then(Node::number);
        assert_eq!(min, Some(-15.0));
        let sym = g.object(ports[0], &format!("{LV2}symbol")).and_then(Node::text);
        assert_eq!(sym, Some("sustain"));
    }

    #[test]
    fn escapes_and_odd_names_survive() {
        let text = "@prefix plug: <https://x.org/surge-xt:> .\n\
                    plug:macro_0 <http://p> \"a \\\"q\\\" b\"^^<http://t> ; <http://n> true .\n\
                    <https://x.org/surge-xt:UI> <http://bin> <libSurge%20XT.so> .";
        let mut g = Graph::default();
        g.parse(text, "file:///b/Surge%20XT.lv2/manifest.ttl").unwrap();
        let s = Node::Iri("https://x.org/surge-xt:macro_0".into());
        assert_eq!(g.object(&s, "http://p").and_then(Node::text), Some("a \"q\" b"));
        assert_eq!(g.object(&s, "http://n").and_then(Node::number), Some(1.0));
        let ui = Node::Iri("https://x.org/surge-xt:UI".into());
        let bin = g.object(&ui, "http://bin").and_then(Node::iri).unwrap();
        assert_eq!(percent_decode(bin), "file:///b/Surge XT.lv2/libSurge XT.so");
    }
}
