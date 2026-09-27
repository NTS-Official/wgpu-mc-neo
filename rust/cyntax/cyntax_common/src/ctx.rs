use codespan_reporting::files::{SimpleFile, SimpleFiles};

pub type File = SimpleFile<String, String>;
/// The entire parsing context
/// This is used to store long term data that is needed between each step, string interning being the most obvious
#[derive(Debug)]
pub struct ParseContext {
    pub files: SimpleFiles<String, String>,
    pub current_file: usize,
}
impl ParseContext {
    pub fn current_file(&self) -> &File {
        self.files.get(self.current_file).unwrap()
    }
    /// Resolve an interned string
    pub fn find_quoted_header(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(name).ok()
    }
    pub fn find_bracketed_header(&self, name: &str) -> Option<String> {
        if let Ok(src) = std::fs::read_to_string(name) {
            return Some(src);
        } else if let Ok(src) =
            std::fs::read_to_string(format!("/usr/lib/gcc/x86_64-linux-gnu/13/include/{}", name))
        {
            return Some(src);
        } else if let Ok(src) = std::fs::read_to_string(format!("/usr/local/include/{}", name)) {
            return Some(src);
        } else if let Ok(src) =
            std::fs::read_to_string(format!("/usr/include/x86_64-linux-gnu/{}", name))
        {
            return Some(src);
        } else if let Ok(src) = std::fs::read_to_string(format!("/usr/include/{}", name)) {
            return Some(src);
        } else {
            None
        }
    }
}
pub trait HasContext {
    fn ctx(&self) -> &ParseContext;
}
pub trait HasMutContext {
    fn ctx_mut(&mut self) -> &mut ParseContext;
}

impl HasContext for ParseContext {
    fn ctx(&self) -> &ParseContext {
        self
    }
}
