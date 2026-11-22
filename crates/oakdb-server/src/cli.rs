//! Stub.
use oakdb_sql::Output;
#[derive(Debug, Default)]
pub struct Splitter;
impl Splitter {
    pub fn new() -> Self {
        Splitter
    }
    pub fn push_line(&mut self, _line: &str) -> Vec<String> {
        todo!()
    }
    pub fn is_empty(&self) -> bool {
        todo!()
    }
    pub fn finish(&mut self) -> Option<String> {
        todo!()
    }
}
pub fn format_output(_sql: &str, _out: &Output) -> String {
    todo!()
}
