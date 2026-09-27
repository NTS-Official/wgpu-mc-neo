use strum_macros::EnumString;

use crate::spanned::Spanned;

#[derive(Debug, Clone, PartialEq)]
pub enum PreprocessingToken {
    Identifier(String),
    /// An identifier that should not be considered for a potential macro invocation
    BlueIdentifier(String),
    StringLiteral(String),
    CharLiteral(String),
    PPNumber(String),
    ControlLine(Vec<Spanned<PreprocessingToken>>),
    Whitespace(Whitespace),
    Punctuator(Punctuator),
    Delimited(Box<Delimited>),
}
#[derive(Debug, Clone, PartialEq)]
pub struct Delimited {
    pub opener: Spanned<PreprocessingToken>,
    pub closer: Spanned<PreprocessingToken>,
    pub inner_tokens: Vec<Spanned<PreprocessingToken>>,
}
impl PreprocessingToken {
    pub fn as_delimited(
        self,
    ) -> (
        Spanned<PreprocessingToken>,
        Spanned<PreprocessingToken>,
        Vec<Spanned<PreprocessingToken>>,
    ) {
        match self {
            PreprocessingToken::Delimited(d) => (d.opener, d.closer, d.inner_tokens),
            _ => panic!(),
        }
    }
}

#[derive(Debug)]
pub enum Directive {
    DefineObject(Spanned<String>, Vec<Spanned<PreprocessingToken>>),
    DefineFunction(
        Spanned<String>,
        Spanned<Vec<Spanned<PreprocessingToken>>>,
        Vec<Spanned<PreprocessingToken>>,
    ),
    Undefine(Spanned<String>),
}
#[derive(Debug, PartialEq, Clone)]
pub enum Whitespace {
    /// ` `
    Space,
    /// \n
    Newline,
    /// \t
    Tab,
}
#[derive(Debug, PartialEq, Clone)]
pub enum Punctuator {
    // Brackets and Parentheses
    LeftBracket,  // [
    RightBracket, // ]
    LeftParen,    // (
    RightParen,   // )
    LeftBrace,    // {
    RightBrace,   // }
    Dot,          // .
    Arrow,        // ->

    // Unary and Increment/Decrement Operators
    PlusPlus,   // ++
    MinusMinus, // --
    Ampersand,  // &
    Asterisk,   // *
    Plus,       // +
    Minus,      // -
    Tilde,      // ~
    Bang,       // !

    // Arithmetic and Bitwise Operators
    Slash,      // /
    Percent,    // %
    LeftLeft,   // <<
    RightRight, // >>
    Left,       // <
    Right,      // >
    LeftEqual,  // <=
    RightEqual, // >=
    EqualEqual, // ==
    BangEqual,  // !=
    Caret,      // ^
    Pipe,       // |
    AndAnd,     // &&
    PipePipe,   // ||

    // Ternary and Colon Operators
    Question,  // ?
    Colon,     // :
    Semicolon, // ;
    DotDotDot, // ...

    // Assignment Operators
    Equal,           // =
    AsteriskEqual,   // *=
    SlashEqual,      // /=
    PercentEqual,    // %=
    PlusEqual,       // +=
    MinusEqual,      // -=
    LeftLeftEqual,   // <<=
    RightRightEqual, // >>=
    AndEqual,        // &=
    CaretEqual,      // ^=
    PipeEqual,       // |=

    // Miscellaneous
    Comma,     // ,
    Directive, // #, but only if its the first non-whitespace character of the line
    Hash,      // #
    HashHash,  // ##

    // Digraphs (Alternative Tokens)
    LeftColon,                // <:
    ColonRight,               // :>
    LessPercent,              // <%
    PercentRight,             // %>
    PercentColon,             // %:
    PercentColonPercentColon, // %:%:
}

#[derive(Debug, Clone, PartialEq, Eq, EnumString, strum_macros::Display)]
#[strum(serialize_all = "lowercase")]
pub enum Keyword {
    Auto,
    Break,
    Case,
    Char,
    Const,
    Continue,
    Default,
    Do,
    Double,
    Else,
    Enum,
    Extern,
    Float,
    For,
    Goto,
    If,
    Inline,
    Int,
    Long,
    Register,
    Restrict,
    Return,
    Short,
    Signed,
    Sizeof,
    Static,
    Struct,
    Switch,
    Typedef,
    Union,
    Unsigned,
    Void,
    Volatile,
    While,
    #[strum(serialize = "_Bool")]
    Bool,
    #[strum(serialize = "_Complex")]
    Complex,
    #[strum(serialize = "_Imaginary")]
    Imaginary,
}

impl Punctuator {
    pub fn to_string(&self) -> String {
        match self {
            Punctuator::LeftBracket => "[".to_string(),
            Punctuator::RightBracket => "]".to_string(),
            Punctuator::LeftParen => "(".to_string(),
            Punctuator::RightParen => ")".to_string(),
            Punctuator::LeftBrace => "{".to_string(),
            Punctuator::RightBrace => "}".to_string(),
            Punctuator::Dot => ".".to_string(),
            Punctuator::Arrow => "->".to_string(),
            Punctuator::PlusPlus => "++".to_string(),
            Punctuator::MinusMinus => "--".to_string(),
            Punctuator::Ampersand => "&".to_string(),
            Punctuator::Asterisk => "*".to_string(),
            Punctuator::Plus => "+".to_string(),
            Punctuator::Minus => "-".to_string(),
            Punctuator::Tilde => "~".to_string(),
            Punctuator::Bang => "!".to_string(),
            Punctuator::Slash => "/".to_string(),
            Punctuator::Percent => "%".to_string(),
            Punctuator::LeftLeft => "<<".to_string(),
            Punctuator::RightRight => ">>".to_string(),
            Punctuator::Left => "<".to_string(),
            Punctuator::Right => ">".to_string(),
            Punctuator::LeftEqual => "<=".to_string(),
            Punctuator::RightEqual => ">=".to_string(),
            Punctuator::EqualEqual => "==".to_string(),
            Punctuator::BangEqual => "!=".to_string(),
            Punctuator::Caret => "^".to_string(),
            Punctuator::Pipe => "|".to_string(),
            Punctuator::AndAnd => "&&".to_string(),
            Punctuator::PipePipe => "||".to_string(),
            Punctuator::Question => "?".to_string(),
            Punctuator::Colon => ":".to_string(),
            Punctuator::Semicolon => ";".to_string(),
            Punctuator::DotDotDot => "...".to_string(),
            Punctuator::Equal => "=".to_string(),
            Punctuator::AsteriskEqual => "*=".to_string(),
            Punctuator::SlashEqual => "/=".to_string(),
            Punctuator::PercentEqual => "%=".to_string(),
            Punctuator::PlusEqual => "+=".to_string(),
            Punctuator::MinusEqual => "-=".to_string(),
            Punctuator::LeftLeftEqual => "<<=".to_string(),
            Punctuator::RightRightEqual => ">>=".to_string(),
            Punctuator::AndEqual => "&=".to_string(),
            Punctuator::CaretEqual => "^=".to_string(),
            Punctuator::PipeEqual => "|=".to_string(),
            Punctuator::Comma => ",".to_string(),
            // Use alternate character `♯` instead of `#` for debug printing, to make it clear that its a Directive not a Hash
            Punctuator::Directive => "♯".to_string(),
            Punctuator::Hash => "#".to_string(),
            Punctuator::HashHash => "##".to_string(),
            Punctuator::LeftColon => "<:".to_string(),
            Punctuator::ColonRight => "".to_string(),
            Punctuator::LessPercent => "<%".to_string(),
            Punctuator::PercentRight => "%>".to_string(),
            Punctuator::PercentColon => "%:".to_string(),
            Punctuator::PercentColonPercentColon => "%:%:".to_string(),
        }
    }
    pub fn is_punctuation(c: char) -> bool {
        Punctuator::from_char(c).is_some()
    }
    pub fn from_char(c: char) -> Option<Punctuator> {
        match c {
            '[' => Some(Punctuator::LeftBracket),
            ']' => Some(Punctuator::RightBracket),
            '(' => Some(Punctuator::LeftParen),
            ')' => Some(Punctuator::RightParen),
            '{' => Some(Punctuator::LeftBrace),
            '}' => Some(Punctuator::RightBrace),
            '.' => Some(Punctuator::Dot),
            '&' => Some(Punctuator::Ampersand),
            '*' => Some(Punctuator::Asterisk),
            '+' => Some(Punctuator::Plus),
            '-' => Some(Punctuator::Minus),
            '~' => Some(Punctuator::Tilde),
            '!' => Some(Punctuator::Bang),
            '/' => Some(Punctuator::Slash),
            '%' => Some(Punctuator::Percent),
            '<' => Some(Punctuator::Left),
            '>' => Some(Punctuator::Right),
            '^' => Some(Punctuator::Caret),
            '|' => Some(Punctuator::Pipe),
            '?' => Some(Punctuator::Question),
            ':' => Some(Punctuator::Colon),
            ';' => Some(Punctuator::Semicolon),
            '=' => Some(Punctuator::Equal),
            ',' => Some(Punctuator::Comma),
            '#' => Some(Punctuator::Hash),
            _ => None,
        }
    }
}
