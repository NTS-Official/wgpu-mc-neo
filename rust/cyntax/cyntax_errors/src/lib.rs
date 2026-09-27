pub use codespan_reporting;
use codespan_reporting::term::termcolor::Ansi;
use cyntax_common::{ctx::HasContext, spanned::Location};
pub enum DiagnosticSeverity {
    Error,
    Warning,
}
pub enum LabelKind {
    Primary,
    Secondary,
    Help,
}
pub struct Label {
    pub kind: LabelKind,
    pub location: Location,
    pub message: String,
}
pub trait Diagnostic: Sized {
    fn title<'a>(&self) -> &'a str;
    fn severity(&self) -> DiagnosticSeverity;
    fn labels(&self) -> Vec<Label> {
        vec![]
    }
    fn into_codespan_report(self) -> codespan_reporting::diagnostic::Diagnostic<usize> {
        let diag = match self.severity() {
            DiagnosticSeverity::Error => codespan_reporting::diagnostic::Diagnostic::error(),
            DiagnosticSeverity::Warning => codespan_reporting::diagnostic::Diagnostic::warning(),
        };
        let mut diag = diag.with_message(self.title());
        for label in self.labels() {
            let style = match label.kind {
                LabelKind::Primary => codespan_reporting::diagnostic::LabelStyle::Primary,
                LabelKind::Secondary => codespan_reporting::diagnostic::LabelStyle::Secondary,
                LabelKind::Help => unimplemented!(),
            };
            diag.labels.push(
                codespan_reporting::diagnostic::Label::new(
                    style,
                    label.location.file_id,
                    label.location.range,
                )
                .with_message(label.message),
            );
        }

        diag
    }
}
impl<T, C: HasContext> UnwrapDiagnostic<T> for C {
    fn unwrap_with_diagnostic<
        F: FnOnce(&mut Self) -> Result<T, codespan_reporting::diagnostic::Diagnostic<usize>>,
    >(
        &mut self,
        value: F,
    ) -> T {
        let result = value(self);
        self.unwrap_diagnostic(result)
    }
    fn unwrap_diagnostic(
        &self,
        value: Result<T, codespan_reporting::diagnostic::Diagnostic<usize>>,
    ) -> T {
        match value {
            Ok(value) => value,
            Err(err) => {
                let config = codespan_reporting::term::Config::default();
                let mut output_buffer = Vec::new();
                let mut ansi_writer = Ansi::new(&mut output_buffer);
                codespan_reporting::term::emit(&mut ansi_writer, &config, &self.ctx().files, &err)
                    .unwrap();

                panic!("{}", String::from_utf8(output_buffer).unwrap())
            }
        }
    }
}
pub mod errors;
pub trait UnwrapDiagnostic<T> {
    fn unwrap_diagnostic(
        &self,
        value: Result<T, codespan_reporting::diagnostic::Diagnostic<usize>>,
    ) -> T;
    fn unwrap_with_diagnostic<
        F: FnOnce(&mut Self) -> Result<T, codespan_reporting::diagnostic::Diagnostic<usize>>,
    >(
        &mut self,
        value: F,
    ) -> T;
}
