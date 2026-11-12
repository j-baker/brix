//! Nix XML byte formatting, independent of runtime value storage.

/// `XMLWriter` with indentation on.
pub(crate) struct XmlWriter {
    pub(crate) out: Vec<u8>,
    open: Vec<String>,
}

impl XmlWriter {
    pub(crate) fn new() -> XmlWriter {
        XmlWriter {
            out: b"<?xml version='1.0' encoding='utf-8'?>\n".to_vec(),
            open: Vec::new(),
        }
    }

    fn indent(&mut self, depth: usize) {
        self.out.extend(std::iter::repeat_n(b' ', depth * 2));
    }

    fn attrs(&mut self, attrs: &[(&str, &[u8])]) {
        // `XMLAttrs` is a `std::map`: attributes come out sorted by name.
        let mut attrs: Vec<&(&str, &[u8])> = attrs.iter().collect();
        attrs.sort_by_key(|(k, _)| *k);
        for (k, v) in attrs {
            self.out.push(b' ');
            self.out.extend_from_slice(k.as_bytes());
            self.out.extend_from_slice(b"=\"");
            for &c in v.iter() {
                match c {
                    b'"' => self.out.extend_from_slice(b"&quot;"),
                    b'<' => self.out.extend_from_slice(b"&lt;"),
                    b'>' => self.out.extend_from_slice(b"&gt;"),
                    b'&' => self.out.extend_from_slice(b"&amp;"),
                    b'\n' => self.out.extend_from_slice(b"&#xA;"),
                    _ => self.out.push(c),
                }
            }
            self.out.push(b'"');
        }
    }

    pub(crate) fn open_element(&mut self, name: &str, attrs: &[(&str, &[u8])]) {
        self.indent(self.open.len());
        self.out.push(b'<');
        self.out.extend_from_slice(name.as_bytes());
        self.attrs(attrs);
        self.out.extend_from_slice(b">\n");
        self.open.push(name.to_owned());
    }

    pub(crate) fn close_element(&mut self) {
        let name = self.open.pop().expect("unbalanced close");
        self.indent(self.open.len());
        self.out.extend_from_slice(b"</");
        self.out.extend_from_slice(name.as_bytes());
        self.out.extend_from_slice(b">\n");
    }

    pub(crate) fn empty_element(&mut self, name: &str, attrs: &[(&str, &[u8])]) {
        self.indent(self.open.len());
        self.out.push(b'<');
        self.out.extend_from_slice(name.as_bytes());
        self.attrs(attrs);
        self.out.extend_from_slice(b" />\n");
    }
}
