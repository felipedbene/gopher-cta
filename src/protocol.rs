//! Gopher protocol (RFC 1436) wire types and serialization.
//!
//! A client sends one selector string terminated by CRLF; the server replies
//! with either a menu (a sequence of tab-delimited item lines, terminated by a
//! line containing a single `.`) or a raw text file (a text block, also
//! terminated by a lone `.` when served as a gopher item). Every line ends with
//! CRLF. Forgetting the trailing dot hangs clients, so the terminator is baked
//! into the builders here and asserted in tests.

/// Gopher item types we use. The wire byte is the leading character of a menu
/// line. We only need the handful below; the rest of the type zoo is unused.
/// `Search` (type 7) and [`split_query`] are the seam for the stretch-goal
/// station search, kept and tested but not yet wired into a selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ItemType {
    Text,   // '0' text file
    Menu,   // '1' submenu / directory
    Info,   // 'i' informational line (not selectable) — used for map rows
    Search, // '7' full-text search (index server)
}

impl ItemType {
    pub fn code(self) -> char {
        match self {
            ItemType::Text => '0',
            ItemType::Menu => '1',
            ItemType::Info => 'i',
            ItemType::Search => '7',
        }
    }
}

/// One line of a gopher menu.
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub itype: ItemType,
    pub display: String,
    pub selector: String,
    pub host: String,
    pub port: u16,
}

impl MenuItem {
    /// A selectable item (text/menu/search) pointing at this server.
    pub fn link(
        itype: ItemType,
        display: impl Into<String>,
        selector: impl Into<String>,
        host: &str,
        port: u16,
    ) -> Self {
        MenuItem {
            itype,
            display: display.into(),
            selector: selector.into(),
            host: host.to_string(),
            port,
        }
    }

    /// An `i`-line: informational text, not a link. By convention the selector
    /// is a placeholder and host/port are dummy values; clients ignore them.
    pub fn info(display: impl Into<String>) -> Self {
        MenuItem {
            itype: ItemType::Info,
            display: display.into(),
            selector: "fake".to_string(),
            host: "(NULL)".to_string(),
            port: 0,
        }
    }

    /// Serialize to `<type><display>\tSELECTOR\tHOST\tPORT\r\n`. Tabs in the
    /// display text would corrupt the field layout, so they're stripped.
    fn to_wire(&self) -> String {
        let display = self.display.replace('\t', " ");
        format!(
            "{}{}\t{}\t{}\t{}\r\n",
            self.itype.code(),
            display,
            self.selector,
            self.host,
            self.port
        )
    }
}

/// Build a full menu payload: every item line followed by the terminating
/// `.\r\n`. This is what gets written to the socket for a type-1 response.
pub fn render_menu(items: &[MenuItem]) -> String {
    let mut out = String::new();
    for item in items {
        out.push_str(&item.to_wire());
    }
    out.push_str(".\r\n");
    out
}

/// Build a text-file payload from a plain body. Lines are normalized to CRLF
/// and the lone-dot terminator is appended. Per RFC 1436, a line in the body
/// that is exactly `.` must be escaped to `..` so it isn't read as the end.
pub fn render_text(body: &str) -> String {
    let mut out = String::new();
    for line in body.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line == "." {
            out.push('.'); // escape -> ".."
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out
}

/// Parse a raw selector line from a client: strip a trailing CRLF (or lone LF),
/// returning the selector. A type-7 query arrives as `selector\tquery`; callers
/// that care split on the first TAB themselves via [`split_query`].
pub fn parse_selector(raw: &str) -> &str {
    raw.strip_suffix("\r\n")
        .or_else(|| raw.strip_suffix('\n'))
        .unwrap_or(raw)
}

/// Split a type-7 selector into `(selector, Some(query))` on the first TAB, or
/// `(selector, None)` when there's no query part. Part of the stretch-goal
/// search seam; tested but not yet routed.
#[allow(dead_code)]
pub fn split_query(selector: &str) -> (&str, Option<&str>) {
    match selector.split_once('\t') {
        Some((sel, q)) => (sel, Some(q)),
        None => (selector, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_line_is_tab_delimited_with_crlf() {
        let item = MenuItem::link(ItemType::Menu, "Map", "/map", "localhost", 7070);
        let wire = item.to_wire();
        assert_eq!(wire, "1Map\t/map\tlocalhost\t7070\r\n");
        assert_eq!(wire.matches('\t').count(), 3);
        assert!(wire.ends_with("\r\n"));
    }

    #[test]
    fn info_line_has_trailing_fields() {
        let wire = MenuItem::info("HELLO").to_wire();
        assert_eq!(wire, "iHELLO\tfake\t(NULL)\t0\r\n");
        assert_eq!(wire.matches('\t').count(), 3);
        assert_eq!(wire.chars().next().unwrap(), 'i');
    }

    #[test]
    fn menu_ends_with_lone_dot() {
        let items = vec![
            MenuItem::link(ItemType::Text, "About", "/about", "h", 70),
            MenuItem::info("note"),
        ];
        let menu = render_menu(&items);
        assert!(menu.ends_with(".\r\n"));
        // The terminator is its own line: preceding char is the item's LF.
        assert!(menu.ends_with("\r\n.\r\n"));
        // Every line ends in CRLF.
        for line in menu.split_inclusive("\r\n") {
            assert!(line.ends_with("\r\n"), "line not CRLF-terminated: {line:?}");
        }
    }

    #[test]
    fn text_payload_normalizes_and_terminates() {
        let body = "line one\nline two";
        let text = render_text(body);
        assert_eq!(text, "line one\r\nline two\r\n.\r\n");
        assert!(text.ends_with(".\r\n"));
    }

    #[test]
    fn text_payload_escapes_lone_dot() {
        let text = render_text("before\n.\nafter");
        assert!(text.contains("\r\n..\r\n"));
        // ends with the real terminator, not the escaped one
        assert!(text.ends_with("after\r\n.\r\n"));
    }

    #[test]
    fn parse_selector_strips_crlf() {
        assert_eq!(parse_selector("/map\r\n"), "/map");
        assert_eq!(parse_selector("/map\n"), "/map");
        assert_eq!(parse_selector("/map"), "/map");
        assert_eq!(parse_selector("\r\n"), ""); // empty selector = root
    }

    #[test]
    fn split_query_splits_on_tab() {
        assert_eq!(split_query("/find\tBelmont"), ("/find", Some("Belmont")));
        assert_eq!(split_query("/find"), ("/find", None));
    }
}
