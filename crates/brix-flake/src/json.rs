//! `nlohmann::json::parse` for the JSON documents Nix's fetchers read (a
//! Git input's `publicKeys`): nlohmann's parser
//! ([`brix_store::json::sax_parse`]) building a [`serde_json::Value`], so
//! that what parses and what the errors say are Nix's.

use brix_store::json::{sax_parse, Sax, SaxError};
use serde_json::{Map, Number, Value};

/// `json::parse(input)`: the document, or nlohmann's exception as an
/// [`crate::Error::Foreign`] (Nix prints it as `error: <what>`, without
/// traces). A key given twice keeps its last value, as in nlohmann's
/// objects.
pub fn parse(input: &[u8]) -> crate::Result<Value> {
    let mut builder = Builder::default();
    match sax_parse(input, &mut builder) {
        Ok(()) => Ok(builder.root.expect("a document that parses holds a value")),
        Err(SaxError::Parse(e)) => Err(crate::Error::Foreign(e.message_lossy())),
        Err(SaxError::Handler(never)) => match never {},
    }
}

/// `json_sax_dom_parser`: the values, built bottom-up.
#[derive(Default)]
struct Builder {
    /// The arrays and objects being read, innermost last; an object with
    /// the key whose value comes next.
    open: Vec<Open>,
    root: Option<Value>,
}

enum Open {
    Array(Vec<Value>),
    Object(Map<String, Value>, Option<String>),
}

/// nlohmann's strings are UTF-8: its lexer refuses anything else.
fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("nlohmann's lexer only lets UTF-8 through")
}

impl Builder {
    fn add(&mut self, value: Value) {
        match self.open.last_mut() {
            Some(Open::Array(items)) => items.push(value),
            Some(Open::Object(members, key)) => {
                let key = key.take().expect("a value in an object follows its key");
                members.insert(key, value);
            }
            None => self.root = Some(value),
        }
    }
}

impl Sax for Builder {
    type Error = std::convert::Infallible;

    fn null(&mut self) -> Result<(), Self::Error> {
        self.add(Value::Null);
        Ok(())
    }

    fn boolean(&mut self, value: bool) -> Result<(), Self::Error> {
        self.add(Value::Bool(value));
        Ok(())
    }

    fn number_integer(&mut self, value: i64) -> Result<(), Self::Error> {
        self.add(Value::from(value));
        Ok(())
    }

    fn number_unsigned(&mut self, value: u64) -> Result<(), Self::Error> {
        self.add(Value::from(value));
        Ok(())
    }

    fn number_float(&mut self, value: f64, _text: &[u8]) -> Result<(), Self::Error> {
        let number = Number::from_f64(value).expect("nlohmann's parser refuses infinite numbers");
        self.add(Value::Number(number));
        Ok(())
    }

    fn string(&mut self, value: &[u8]) -> Result<(), Self::Error> {
        self.add(Value::String(text(value)));
        Ok(())
    }

    fn start_object(&mut self) -> Result<(), Self::Error> {
        self.open.push(Open::Object(Map::new(), None));
        Ok(())
    }

    fn key(&mut self, key: &[u8]) -> Result<(), Self::Error> {
        if let Some(Open::Object(_, pending)) = self.open.last_mut() {
            *pending = Some(text(key));
        }
        Ok(())
    }

    fn end_object(&mut self) -> Result<(), Self::Error> {
        if let Some(Open::Object(members, _)) = self.open.pop() {
            self.add(Value::Object(members));
        }
        Ok(())
    }

    fn start_array(&mut self) -> Result<(), Self::Error> {
        self.open.push(Open::Array(Vec::new()));
        Ok(())
    }

    fn end_array(&mut self) -> Result<(), Self::Error> {
        if let Some(Open::Array(items)) = self.open.pop() {
            self.add(Value::Array(items));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_parse_as_nlohmann_reads_them() {
        assert_eq!(
            parse(br#"{"a": [1, -2, 2.5, "x", null, true], "a": {"b": {}}}"#).unwrap(),
            serde_json::json!({"a": {"b": {}}})
        );
        // A byte order mark is skipped, and a NUL byte ends the input.
        assert_eq!(
            parse(b"\xEF\xBB\xBF[1]\0x").unwrap(),
            serde_json::json!([1])
        );
        let err = parse(b"[1, tru]").unwrap_err();
        assert!(err.is_foreign());
        assert_eq!(
            err.to_string(),
            "[json.exception.parse_error.101] parse error at line 1, column 8: syntax error while parsing value - invalid literal; last read: '1, tru]'"
        );
    }
}
