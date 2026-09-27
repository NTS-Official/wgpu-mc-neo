macro_rules! storage_class {
    () => {
        Keyword::Typedef | Keyword::Extern | Keyword::Static | Keyword::Auto | Keyword::Register
    };
}
macro_rules! type_specifier {
    () => {
        Keyword::Void
            | Keyword::Char
            | Keyword::Short
            | Keyword::Int
            | Keyword::Long
            | Keyword::Float
            | Keyword::Double
            | Keyword::Signed
            | Keyword::Unsigned
            | Keyword::Bool
            | Keyword::Complex
            | Keyword::Struct
            | Keyword::Union
            | Keyword::Enum
    };
}
macro_rules! type_qualifier {
    () => {
        Keyword::Const | Keyword::Restrict | Keyword::Volatile
    };
}
