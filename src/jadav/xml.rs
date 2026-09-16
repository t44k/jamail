//! WebDAV/CalDAV XML for the server: request-body parsing into a small
//! namespace-resolved DOM, typed views for `PROPFIND`/`PROPPATCH`/
//! `MKCALENDAR`/`REPORT`, and a multistatus response builder.
//!
//! Requests are parsed with `quick_xml::NsReader`, which resolves prefixes
//! to namespace URIs, so `<propfind xmlns="DAV:">` and
//! `<D:propfind xmlns:D="DAV:">` both arrive as `{DAV:}propfind` — no
//! prefix guessing. Text accumulates from `Text`, `CData` *and*
//! `GeneralRef` events (quick-xml ≥ 0.38 reports `&amp;`-style references
//! separately; dropping them was the bug that once unquoted every ETag in
//! the client — see `caldav::general_ref_text`).
//!
//! Responses are string-formatted: the shape is fully server-controlled and
//! the only dynamic content is text (hrefs, ICS, names) and, in a 404
//! propstat, a client-supplied namespace in an attribute — two escapers
//! cover that.

use crate::caldav::general_ref_text;
use anyhow::{Context, Result, bail};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;

pub const NS_DAV: &str = "DAV:";
pub const NS_CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub const NS_CS: &str = "http://calendarserver.org/ns/";
pub const NS_APPLE: &str = "http://apple.com/ns/ical/";

const MAX_DEPTH: usize = 32;
const MAX_BODY: usize = 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct QName {
    pub ns: String,
    pub local: String,
}

impl QName {
    pub fn new(ns: &str, local: &str) -> Self {
        Self {
            ns: ns.to_string(),
            local: local.to_string(),
        }
    }
    pub fn dav(local: &str) -> Self {
        Self::new(NS_DAV, local)
    }
    pub fn caldav(local: &str) -> Self {
        Self::new(NS_CALDAV, local)
    }
    pub fn cs(local: &str) -> Self {
        Self::new(NS_CS, local)
    }
    pub fn apple(local: &str) -> Self {
        Self::new(NS_APPLE, local)
    }
    pub fn is(&self, ns: &str, local: &str) -> bool {
        self.ns == ns && self.local == local
    }
}

#[derive(Clone, Debug, Default)]
pub struct Element {
    pub name: QName,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    pub text: String,
}

impl Element {
    pub fn child(&self, ns: &str, local: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name.is(ns, local))
    }
    pub fn children_named<'a>(
        &'a self,
        ns: &'a str,
        local: &'a str,
    ) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name.is(ns, local))
    }
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    pub fn text_trim(&self) -> &str {
        self.text.trim()
    }
}

/// Parse a request body into an element tree. Depth and size are capped
/// so a hostile body cannot exhaust the server.
pub fn parse_element_tree(body: &[u8]) -> Result<Element> {
    if body.len() > MAX_BODY {
        bail!("XML body too large ({} bytes)", body.len());
    }
    let mut reader = quick_xml::NsReader::from_reader(body);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    loop {
        let (ns, event) = reader
            .read_resolved_event()
            .context("parsing WebDAV request XML")?;
        match event {
            Event::Start(e) => {
                if stack.len() >= MAX_DEPTH {
                    bail!("XML nesting too deep");
                }
                let ns_uri = match ns {
                    ResolveResult::Bound(n) => n.as_ref().to_string(),
                    _ => String::new(),
                };
                let local = e.local_name().as_ref().to_string();
                let mut attrs = Vec::new();
                for attr in e.attributes().flatten() {
                    let key = attr.key.local_name().as_ref().to_string();
                    let raw_value: &str = attr.value.as_ref();
                    let value = quick_xml::escape::unescape(raw_value)
                        .map(|v| v.into_owned())
                        .unwrap_or_else(|_| raw_value.to_string());
                    attrs.push((key, value));
                }
                stack.push(Element {
                    name: QName { ns: ns_uri, local },
                    attrs,
                    children: Vec::new(),
                    text: String::new(),
                });
            }
            Event::End(_) => {
                let Some(done) = stack.pop() else { break };
                match stack.last_mut() {
                    Some(parent) => parent.children.push(done),
                    None => {
                        root = Some(done);
                        break;
                    }
                }
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    let raw: &str = t.as_ref();
                    match quick_xml::escape::unescape(raw) {
                        Ok(decoded) => top.text.push_str(&decoded),
                        Err(_) => top.text.push_str(raw),
                    }
                }
            }
            Event::GeneralRef(r) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&general_ref_text(&r));
                }
            }
            Event::CData(c) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(c.as_ref());
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    root.context("XML body has no root element")
}

// ---------------------------------------------------------------------
// Typed request views
// ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropfindRequest {
    Prop(Vec<QName>),
    AllProp,
    PropName,
}

/// An empty body is `allprop` (RFC 4918 §9.1).
pub fn parse_propfind(body: &[u8]) -> Result<PropfindRequest> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(PropfindRequest::AllProp);
    }
    let root = parse_element_tree(body)?;
    if !root.name.is(NS_DAV, "propfind") {
        bail!("PROPFIND body root is not DAV:propfind");
    }
    if root.child(NS_DAV, "allprop").is_some() {
        return Ok(PropfindRequest::AllProp);
    }
    if root.child(NS_DAV, "propname").is_some() {
        return Ok(PropfindRequest::PropName);
    }
    match root.child(NS_DAV, "prop") {
        Some(prop) => Ok(PropfindRequest::Prop(
            prop.children.iter().map(|c| c.name.clone()).collect(),
        )),
        None => Ok(PropfindRequest::AllProp),
    }
}

/// The value of a `<D:prop>` child in `PROPPATCH`/`MKCALENDAR`: its text
/// and the names of any element children (`<C:opaque/>` etc.).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropValue {
    pub text: String,
    pub child_names: Vec<QName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropOp {
    Set(QName, PropValue),
    Remove(QName),
}

fn collect_set_remove(root: &Element) -> Vec<PropOp> {
    let mut ops = Vec::new();
    for child in &root.children {
        let is_set = child.name.is(NS_DAV, "set");
        let is_remove = child.name.is(NS_DAV, "remove");
        if !is_set && !is_remove {
            continue;
        }
        for prop in child.children_named(NS_DAV, "prop") {
            for p in &prop.children {
                if is_set {
                    ops.push(PropOp::Set(
                        p.name.clone(),
                        PropValue {
                            text: p.text.clone(),
                            child_names: p.children.iter().map(|c| c.name.clone()).collect(),
                        },
                    ));
                } else {
                    ops.push(PropOp::Remove(p.name.clone()));
                }
            }
        }
    }
    ops
}

pub fn parse_proppatch(body: &[u8]) -> Result<Vec<PropOp>> {
    let root = parse_element_tree(body)?;
    if !root.name.is(NS_DAV, "propertyupdate") {
        bail!("PROPPATCH body root is not DAV:propertyupdate");
    }
    Ok(collect_set_remove(&root))
}

/// `MKCALENDAR` (`C:mkcalendar`) and extended `MKCOL` (`D:mkcol`) carry the
/// same `<D:set><D:prop>` shape; an empty body creates with defaults.
/// Returns the ops and whether an extended-MKCOL body declared a
/// `C:calendar` resourcetype.
pub fn parse_mkcalendar(body: &[u8]) -> Result<(Vec<PropOp>, bool)> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok((Vec::new(), false));
    }
    let root = parse_element_tree(body)?;
    if !root.name.is(NS_CALDAV, "mkcalendar") && !root.name.is(NS_DAV, "mkcol") {
        bail!("MKCALENDAR body root is neither C:mkcalendar nor D:mkcol");
    }
    let ops = collect_set_remove(&root);
    let declares_calendar = ops.iter().any(|op| match op {
        PropOp::Set(name, value) if name.is(NS_DAV, "resourcetype") => value
            .child_names
            .iter()
            .any(|c| c.is(NS_CALDAV, "calendar")),
        _ => false,
    });
    Ok((ops, declares_calendar))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportRequest {
    Multiget {
        props: Vec<QName>,
        hrefs: Vec<String>,
    },
    Query {
        props: Vec<QName>,
        /// A `comp-filter` naming something other than `VEVENT` (nothing
        /// matches; the server stores VEVENTs only).
        other_component: Option<String>,
        /// `(start, end)` UTC seconds from a `time-range` on the VEVENT
        /// filter; either side may be open.
        time_range: Option<(Option<i64>, Option<i64>)>,
    },
    SyncCollection {
        token: Option<String>,
        props: Vec<QName>,
    },
    Unsupported(QName),
}

fn prop_names(root: &Element) -> Vec<QName> {
    root.child(NS_DAV, "prop")
        .map(|p| p.children.iter().map(|c| c.name.clone()).collect())
        .unwrap_or_default()
}

/// Parse an iCalendar UTC date-time (`20240115T090000Z`) or date
/// (`20240115`) as used in `time-range` attributes.
pub fn parse_utc_stamp(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y%m%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp());
    }
    None
}

pub fn parse_report(body: &[u8]) -> Result<ReportRequest> {
    let root = parse_element_tree(body)?;
    if root.name.is(NS_CALDAV, "calendar-multiget") {
        let hrefs = root
            .children_named(NS_DAV, "href")
            .map(|h| h.text_trim().to_string())
            .filter(|h| !h.is_empty())
            .collect();
        return Ok(ReportRequest::Multiget {
            props: prop_names(&root),
            hrefs,
        });
    }
    if root.name.is(NS_CALDAV, "calendar-query") {
        let mut other_component = None;
        let mut time_range = None;
        if let Some(filter) = root.child(NS_CALDAV, "filter") {
            for outer in filter.children_named(NS_CALDAV, "comp-filter") {
                for inner in outer.children_named(NS_CALDAV, "comp-filter") {
                    let name = inner.attr("name").unwrap_or("").to_ascii_uppercase();
                    if name != "VEVENT" {
                        other_component = Some(name);
                        continue;
                    }
                    if let Some(tr) = inner.child(NS_CALDAV, "time-range") {
                        let start = tr.attr("start").and_then(parse_utc_stamp);
                        let end = tr.attr("end").and_then(parse_utc_stamp);
                        if start.is_some() || end.is_some() {
                            time_range = Some((start, end));
                        }
                    }
                }
            }
        }
        return Ok(ReportRequest::Query {
            props: prop_names(&root),
            other_component,
            time_range,
        });
    }
    if root.name.is(NS_DAV, "sync-collection") {
        let token = root
            .child(NS_DAV, "sync-token")
            .map(|t| t.text_trim().to_string())
            .filter(|t| !t.is_empty());
        return Ok(ReportRequest::SyncCollection {
            token,
            props: prop_names(&root),
        });
    }
    Ok(ReportRequest::Unsupported(root.name))
}

// ---------------------------------------------------------------------
// Response building
// ---------------------------------------------------------------------

pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn reason_phrase(code: u16) -> &'static str {
    match code {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        207 => "Multi-Status",
        301 => "Moved Permanently",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        424 => "Failed Dependency",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        507 => "Insufficient Storage",
        _ => "Unknown",
    }
}

pub fn status_line(code: u16) -> String {
    format!("HTTP/1.1 {} {}", code, reason_phrase(code))
}

/// The prefix this server uses for a namespace in its responses, or `None`
/// for a foreign namespace (rendered with an inline `xmlns`).
pub fn prefix_for(ns: &str) -> Option<&'static str> {
    match ns {
        NS_DAV => Some("D"),
        NS_CALDAV => Some("C"),
        NS_CS => Some("CS"),
        NS_APPLE => Some("A"),
        _ => None,
    }
}

/// Render `<prefix:local>inner</prefix:local>` (or an empty element when
/// `inner` is empty), using an inline namespace declaration for
/// namespaces this server has no prefix for.
pub fn prop_element(name: &QName, inner: &str) -> String {
    let local = escape_attr(&name.local);
    match prefix_for(&name.ns) {
        Some(p) if inner.is_empty() => format!("<{}:{}/>", p, local),
        Some(p) => format!("<{}:{}>{}</{}:{}>", p, local, inner, p, local),
        None if inner.is_empty() => format!("<{} xmlns=\"{}\"/>", local, escape_attr(&name.ns)),
        None => format!(
            "<{} xmlns=\"{}\">{}</{}>",
            local,
            escape_attr(&name.ns),
            inner,
            local
        ),
    }
}

pub const MULTISTATUS_OPEN: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:CS=\"http://calendarserver.org/ns/\" xmlns:A=\"http://apple.com/ns/ical/\">";
pub const MULTISTATUS_CLOSE: &str = "</D:multistatus>";

#[derive(Default)]
pub struct MultiStatus {
    buf: String,
}

impl MultiStatus {
    pub fn new() -> Self {
        Self {
            buf: String::from(MULTISTATUS_OPEN),
        }
    }

    /// Start a `<D:response>` for `href` (already percent-encoded).
    pub fn response(&mut self, href: &str) -> ResponseBuilder<'_> {
        self.buf.push_str("<D:response><D:href>");
        self.buf.push_str(&escape_text(href));
        self.buf.push_str("</D:href>");
        ResponseBuilder { ms: self }
    }

    /// A response carrying only a status (a 404 tombstone in
    /// `sync-collection`, an unknown href in `multiget`).
    pub fn status_only(&mut self, href: &str, code: u16) {
        self.buf.push_str("<D:response><D:href>");
        self.buf.push_str(&escape_text(href));
        self.buf.push_str("</D:href><D:status>");
        self.buf.push_str(&status_line(code));
        self.buf.push_str("</D:status></D:response>");
    }

    pub fn sync_token(&mut self, token: &str) {
        self.buf.push_str("<D:sync-token>");
        self.buf.push_str(&escape_text(token));
        self.buf.push_str("</D:sync-token>");
    }

    pub fn finish(mut self) -> String {
        self.buf.push_str(MULTISTATUS_CLOSE);
        self.buf
    }
}

pub struct ResponseBuilder<'a> {
    ms: &'a mut MultiStatus,
}

impl ResponseBuilder<'_> {
    /// Add a `<D:propstat>` with `inner` (already-rendered prop elements)
    /// and `code`. Skipped when `inner` is empty.
    pub fn propstat(self, code: u16, inner: &str) -> Self {
        self.propstat_with_error(code, inner, None)
    }

    /// Like [`Self::propstat`] with an optional `<D:error>` precondition
    /// element explaining a failure status.
    pub fn propstat_with_error(self, code: u16, inner: &str, error_xml: Option<&str>) -> Self {
        if !inner.is_empty() {
            self.ms.buf.push_str("<D:propstat><D:prop>");
            self.ms.buf.push_str(inner);
            self.ms.buf.push_str("</D:prop><D:status>");
            self.ms.buf.push_str(&status_line(code));
            self.ms.buf.push_str("</D:status>");
            if let Some(err) = error_xml {
                self.ms.buf.push_str("<D:error>");
                self.ms.buf.push_str(err);
                self.ms.buf.push_str("</D:error>");
            }
            self.ms.buf.push_str("</D:propstat>");
        }
        self
    }

    pub fn end(self) {
        self.ms.buf.push_str("</D:response>");
    }
}

/// A `<D:error>` body carrying one precondition/postcondition element.
pub fn dav_error(condition_xml: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:error xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\">{}</D:error>",
        condition_xml
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prefixed_and_default_namespace_propfind() {
        let prefixed = br#"<?xml version="1.0"?><D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:displayname/><C:calendar-home-set/><X:foo xmlns:X="http://example.com/ns"/></D:prop></D:propfind>"#;
        match parse_propfind(prefixed).unwrap() {
            PropfindRequest::Prop(names) => {
                assert_eq!(names.len(), 3);
                assert!(names[0].is(NS_DAV, "displayname"));
                assert!(names[1].is(NS_CALDAV, "calendar-home-set"));
                assert!(names[2].is("http://example.com/ns", "foo"));
            }
            other => panic!("{other:?}"),
        }
        let default_ns = br#"<propfind xmlns="DAV:"><prop><getetag/></prop></propfind>"#;
        match parse_propfind(default_ns).unwrap() {
            PropfindRequest::Prop(names) => assert!(names[0].is(NS_DAV, "getetag")),
            other => panic!("{other:?}"),
        }
        assert_eq!(parse_propfind(b"").unwrap(), PropfindRequest::AllProp);
        assert_eq!(
            parse_propfind(br#"<D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>"#).unwrap(),
            PropfindRequest::AllProp
        );
        assert_eq!(
            parse_propfind(br#"<D:propfind xmlns:D="DAV:"><D:propname/></D:propfind>"#).unwrap(),
            PropfindRequest::PropName
        );
    }

    #[test]
    fn proppatch_text_resolves_entity_references_and_children() {
        let body = br#"<D:propertyupdate xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:A="http://apple.com/ns/ical/"><D:set><D:prop><D:displayname>Tom &amp; Jerry &#38; co</D:displayname><A:calendar-color>#FF0000FF</A:calendar-color><C:schedule-calendar-transp><C:transparent/></C:schedule-calendar-transp></D:prop></D:set><D:remove><D:prop><C:calendar-description/></D:prop></D:remove></D:propertyupdate>"#;
        let ops = parse_proppatch(body).unwrap();
        assert_eq!(ops.len(), 4);
        match &ops[0] {
            PropOp::Set(n, v) => {
                assert!(n.is(NS_DAV, "displayname"));
                assert_eq!(v.text, "Tom & Jerry & co");
            }
            other => panic!("{other:?}"),
        }
        match &ops[2] {
            PropOp::Set(n, v) => {
                assert!(n.is(NS_CALDAV, "schedule-calendar-transp"));
                assert!(v.child_names[0].is(NS_CALDAV, "transparent"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&ops[3], PropOp::Remove(n) if n.is(NS_CALDAV, "calendar-description")));
    }

    #[test]
    fn mkcalendar_and_extended_mkcol_bodies() {
        let mk = br#"<C:mkcalendar xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:set><D:prop><D:displayname>Work</D:displayname></D:prop></D:set></C:mkcalendar>"#;
        let (ops, declares) = parse_mkcalendar(mk).unwrap();
        assert_eq!(ops.len(), 1);
        assert!(!declares);
        let mkcol = br#"<D:mkcol xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:set><D:prop><D:resourcetype><D:collection/><C:calendar/></D:resourcetype><D:displayname>W</D:displayname></D:prop></D:set></D:mkcol>"#;
        let (ops, declares) = parse_mkcalendar(mkcol).unwrap();
        assert_eq!(ops.len(), 2);
        assert!(declares);
        assert_eq!(parse_mkcalendar(b"  ").unwrap().0.len(), 0);
    }

    #[test]
    fn report_kinds_and_time_range() {
        let multiget = br#"<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:getetag/><C:calendar-data/></D:prop><D:href>/dav/calendars/a/p/x.ics</D:href><D:href>/dav/calendars/a/p/y.ics</D:href></C:calendar-multiget>"#;
        match parse_report(multiget).unwrap() {
            ReportRequest::Multiget { props, hrefs } => {
                assert_eq!(props.len(), 2);
                assert_eq!(
                    hrefs,
                    vec!["/dav/calendars/a/p/x.ics", "/dav/calendars/a/p/y.ics"]
                );
            }
            other => panic!("{other:?}"),
        }
        let query = br#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:getetag/></D:prop><C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT"><C:time-range start="20240115T000000Z" end="20240116T000000Z"/></C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"#;
        match parse_report(query).unwrap() {
            ReportRequest::Query {
                time_range,
                other_component,
                ..
            } => {
                assert_eq!(other_component, None);
                let (s, e) = time_range.unwrap();
                assert_eq!(s, Some(1705276800));
                assert_eq!(e, Some(1705363200));
            }
            other => panic!("{other:?}"),
        }
        let todo = br#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VTODO"/></C:comp-filter></C:filter></C:calendar-query>"#;
        assert!(matches!(
            parse_report(todo).unwrap(),
            ReportRequest::Query { other_component: Some(c), .. } if c == "VTODO"
        ));
        let sync = br#"<D:sync-collection xmlns:D="DAV:"><D:sync-token>urn:jadav:p:5</D:sync-token><D:sync-level>1</D:sync-level><D:prop><D:getetag/></D:prop></D:sync-collection>"#;
        assert!(matches!(
            parse_report(sync).unwrap(),
            ReportRequest::SyncCollection { token: Some(t), .. } if t == "urn:jadav:p:5"
        ));
        let initial = br#"<D:sync-collection xmlns:D="DAV:"><D:sync-token/><D:prop><D:getetag/></D:prop></D:sync-collection>"#;
        assert!(matches!(
            parse_report(initial).unwrap(),
            ReportRequest::SyncCollection { token: None, .. }
        ));
        let other = br#"<D:expand-property xmlns:D="DAV:"/>"#;
        assert!(
            matches!(parse_report(other).unwrap(), ReportRequest::Unsupported(n) if n.local == "expand-property")
        );
    }

    #[test]
    fn escaping_and_prop_element_rendering() {
        assert_eq!(escape_text("a<b>&\"c\""), "a&lt;b&gt;&amp;\"c\"");
        assert_eq!(escape_attr("a\"b'c"), "a&quot;b&apos;c");
        assert_eq!(
            prop_element(&QName::dav("displayname"), "x"),
            "<D:displayname>x</D:displayname>"
        );
        assert_eq!(
            prop_element(&QName::caldav("calendar-data"), ""),
            "<C:calendar-data/>"
        );
        assert_eq!(
            prop_element(&QName::new("http://example.com/ns", "foo"), ""),
            "<foo xmlns=\"http://example.com/ns\"/>"
        );
        let mut ms = MultiStatus::new();
        ms.response("/dav/x/")
            .propstat(200, &prop_element(&QName::dav("displayname"), "Q&amp;A"))
            .propstat(404, "")
            .end();
        ms.status_only("/dav/y.ics", 404);
        ms.sync_token("urn:jadav:x:1");
        let out = ms.finish();
        assert!(out.starts_with(MULTISTATUS_OPEN));
        assert!(out.contains("<D:response><D:href>/dav/x/</D:href><D:propstat><D:prop><D:displayname>Q&amp;A</D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"));
        assert!(
            out.contains("<D:href>/dav/y.ics</D:href><D:status>HTTP/1.1 404 Not Found</D:status>")
        );
        assert!(out.ends_with("<D:sync-token>urn:jadav:x:1</D:sync-token></D:multistatus>"));
    }

    #[test]
    fn rejects_oversized_and_too_deep_bodies() {
        let big = vec![b' '; MAX_BODY + 1];
        assert!(parse_element_tree(&big).is_err());
        let mut deep = String::new();
        for _ in 0..(MAX_DEPTH + 2) {
            deep.push_str("<a>");
        }
        assert!(parse_element_tree(deep.as_bytes()).is_err());
    }
}
