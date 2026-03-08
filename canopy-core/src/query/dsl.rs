//! Query AST and S-expression DSL parser.

use crate::error::CanopyError;

/// Query AST
#[derive(Debug, Clone)]
pub enum Query {
    /// (section "heading") - fuzzy match on section headings
    Section(String),
    /// (grep "pattern") - FTS5 search
    Grep(String),
    /// (file "path") - entire file as handle
    File(String),
    /// (code "symbol") - AST symbol search
    Code(String),
    /// (in-file "glob" query) - search within specific files
    InFile(String, Box<Query>),
    /// (union q1 q2 ...) - combine results
    Union(Vec<Query>),
    /// (intersect q1 q2 ...) - intersection of results
    Intersect(Vec<Query>),
    /// (limit N query) - limit results
    Limit(usize, Box<Query>),
    /// (children "parent") - get all children of a parent symbol
    Children(String),
    /// (children-named "parent" "symbol") - get named children of a parent
    ChildrenNamed(String, String),
    /// (definition "symbol") - exact match symbol definition
    Definition(String),
    /// (references "symbol") - find references to a symbol
    References(String),
}

/// Parse a query string into a Query AST
pub fn parse_query(input: &str) -> crate::Result<Query> {
    let input = input.trim();
    if input.is_empty() {
        return Err(CanopyError::QueryParse {
            position: 0,
            message: "Empty query".to_string(),
        });
    }

    let mut parser = QueryParser::new(input);
    parser.parse()
}

/// S-expression parser for the query DSL
struct QueryParser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> QueryParser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn parse(&mut self) -> crate::Result<Query> {
        self.skip_whitespace();

        if self.peek() != Some('(') {
            return Err(self.error("Expected '('"));
        }
        self.advance(); // consume '('

        self.skip_whitespace();

        // Parse the operator
        let op = self.parse_identifier()?;

        let query = match op.as_str() {
            "section" => {
                self.skip_whitespace();
                let arg = self.parse_string()?;
                Query::Section(arg)
            }
            "grep" => {
                self.skip_whitespace();
                let arg = self.parse_string()?;
                Query::Grep(arg)
            }
            "file" => {
                self.skip_whitespace();
                let arg = self.parse_string()?;
                Query::File(arg)
            }
            "code" => {
                self.skip_whitespace();
                let arg = self.parse_string()?;
                Query::Code(arg)
            }
            "in-file" => {
                self.skip_whitespace();
                let glob = self.parse_string()?;
                self.skip_whitespace();
                let subquery = self.parse()?;
                Query::InFile(glob, Box::new(subquery))
            }
            "union" => {
                let mut queries = Vec::new();
                loop {
                    self.skip_whitespace();
                    if self.peek() == Some(')') {
                        break;
                    }
                    queries.push(self.parse()?);
                }
                Query::Union(queries)
            }
            "intersect" => {
                let mut queries = Vec::new();
                loop {
                    self.skip_whitespace();
                    if self.peek() == Some(')') {
                        break;
                    }
                    queries.push(self.parse()?);
                }
                Query::Intersect(queries)
            }
            "limit" => {
                self.skip_whitespace();
                let n = self.parse_number()?;
                self.skip_whitespace();
                let subquery = self.parse()?;
                Query::Limit(n, Box::new(subquery))
            }
            "children" => {
                self.skip_whitespace();
                let parent = self.parse_string()?;
                Query::Children(parent)
            }
            "children-named" => {
                self.skip_whitespace();
                let parent = self.parse_string()?;
                self.skip_whitespace();
                let symbol = self.parse_string()?;
                Query::ChildrenNamed(parent, symbol)
            }
            "definition" => {
                self.skip_whitespace();
                let symbol = self.parse_string()?;
                Query::Definition(symbol)
            }
            "references" => {
                self.skip_whitespace();
                let symbol = self.parse_string()?;
                Query::References(symbol)
            }
            _ => return Err(self.error(&format!("Unknown operator: {}", op))),
        };

        self.skip_whitespace();

        if self.peek() != Some(')') {
            return Err(self.error("Expected ')'"));
        }
        self.advance(); // consume ')'

        Ok(query)
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn parse_identifier(&mut self) -> crate::Result<String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                self.advance();
            } else {
                break;
            }
        }

        let ident = &self.input[start..self.pos];
        if ident.is_empty() {
            Err(self.error("Expected identifier"))
        } else {
            Ok(ident.to_string())
        }
    }

    fn parse_string(&mut self) -> crate::Result<String> {
        if self.peek() != Some('"') {
            return Err(self.error("Expected '\"'"));
        }
        self.advance(); // consume opening quote

        let mut result = String::new();
        let mut escaped = false;

        loop {
            match self.advance() {
                None => return Err(self.error("Unterminated string")),
                Some('\\') if !escaped => {
                    escaped = true;
                }
                Some('"') if !escaped => {
                    break;
                }
                Some(c) => {
                    if escaped {
                        match c {
                            'n' => result.push('\n'),
                            't' => result.push('\t'),
                            'r' => result.push('\r'),
                            _ => result.push(c),
                        }
                        escaped = false;
                    } else {
                        result.push(c);
                    }
                }
            }
        }

        Ok(result)
    }

    fn parse_number(&mut self) -> crate::Result<usize> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }

        let num_str = &self.input[start..self.pos];
        num_str.parse().map_err(|_| self.error("Expected number"))
    }

    fn error(&self, message: &str) -> CanopyError {
        CanopyError::QueryParse {
            position: self.pos,
            message: message.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_input_returns_error() {
        let err = parse_query("").unwrap_err();
        assert!(
            matches!(err, CanopyError::QueryParse { position: 0, ref message } if message.contains("Empty"))
        );

        let err = parse_query("   ").unwrap_err();
        assert!(
            matches!(err, CanopyError::QueryParse { position: 0, ref message } if message.contains("Empty"))
        );
    }

    #[test]
    fn parse_string_with_escape_sequences() {
        // Verify all supported escape sequences: \n, \t, \r, \\, \"
        let q = parse_query(r#"(grep "a\nb\tc\rd\\e\"f")"#).unwrap();
        match q {
            Query::Grep(s) => assert_eq!(s, "a\nb\tc\rd\\e\"f"),
            _ => panic!("expected Grep"),
        }
    }

    #[test]
    fn parse_children_named_extracts_both_args() {
        let q = parse_query(r#"(children-named "MyClass" "do_work")"#).unwrap();
        match q {
            Query::ChildrenNamed(parent, symbol) => {
                assert_eq!(parent, "MyClass");
                assert_eq!(symbol, "do_work");
            }
            _ => panic!("expected ChildrenNamed"),
        }
    }

    #[test]
    fn parse_deeply_nested_structure() {
        let input = r#"(limit 3 (in-file "*.rs" (union (grep "alpha") (code "beta") (definition "gamma"))))"#;
        let q = parse_query(input).unwrap();
        match q {
            Query::Limit(3, inner) => match *inner {
                Query::InFile(ref glob, ref sub) => {
                    assert_eq!(glob, "*.rs");
                    match sub.as_ref() {
                        Query::Union(qs) => {
                            assert_eq!(qs.len(), 3);
                            assert!(matches!(&qs[0], Query::Grep(s) if s == "alpha"));
                            assert!(matches!(&qs[1], Query::Code(s) if s == "beta"));
                            assert!(matches!(&qs[2], Query::Definition(s) if s == "gamma"));
                        }
                        _ => panic!("expected Union"),
                    }
                }
                _ => panic!("expected InFile"),
            },
            _ => panic!("expected Limit"),
        }
    }
}
