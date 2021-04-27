use std::io;
use std::ops::RangeFrom;

use crate::ast::*;

use super::lexer::{Position, Token};

/// A `Result` type alias for parser errors.
pub type ParserResult<T> = Result<T, ParserError>;

#[derive(Debug, PartialEq)]
pub struct ParserError(pub ErrorKind, pub Position);

impl From<(io::Error, Position)> for ParserError {
    fn from((err, pos): (io::Error, Position)) -> Self {
        ParserError(err.into(), pos)
    }
}

/// The error type for the parser and lexer.
#[derive(Debug, PartialEq)]
pub enum ErrorKind {
    Io(ParserIoError),
    UnexpectedChar(Option<char>),
    LeadingZero(String),
    BackslashInQuotedSymbol,
    EofInQuotedSymbol,
    EofInString,
    UnexpectedToken(Token),
    EmptySequence,
    SortError(SortError),
    UndefinedIden(Identifier),
    UndefinedStepIndex(String),
    WrongNumberOfArgs(usize, usize),
    RepeatedStepIndex,
    NotYetImplemented,
}

impl From<io::Error> for ErrorKind {
    fn from(err: io::Error) -> Self {
        ErrorKind::Io(ParserIoError(err))
    }
}

impl From<SortError> for ErrorKind {
    fn from(err: SortError) -> Self {
        ErrorKind::SortError(err)
    }
}

impl ErrorKind {
    /// Returns an error if the length of `sequence` is not `expected`.
    pub fn assert_num_of_args<T>(sequence: &[T], expected: usize) -> Result<(), Self> {
        let got = sequence.len();
        if got == expected {
            Ok(())
        } else {
            Err(ErrorKind::WrongNumberOfArgs(expected, got))
        }
    }

    pub fn assert_num_of_args_range<T>(
        sequence: &[T],
        expected: RangeFrom<usize>,
    ) -> Result<(), Self> {
        let got = sequence.len();
        if expected.contains(&got) {
            Ok(())
        } else {
            Err(ErrorKind::WrongNumberOfArgs(expected.start, got))
        }
    }
}

/// A simple wrapper of io::Error so ParserError can derive PartialEq
#[derive(Debug)]
pub struct ParserIoError(io::Error);

impl PartialEq for ParserIoError {
    fn eq(&self, other: &Self) -> bool {
        self.0.kind() == other.0.kind()
    }
}

#[derive(Debug, PartialEq)]
pub enum SortError {
    Expected { expected: Term, got: Term },
    ExpectedOneOf { possibilities: Vec<Term>, got: Term },
}

impl SortError {
    /// Returns an `Expected` sort error if `got` does not equal `expected`.
    pub fn assert_eq(expected: &Term, got: &Term) -> Result<(), Self> {
        if expected == got {
            Ok(())
        } else {
            Err(Self::Expected {
                expected: expected.clone(),
                got: got.clone(),
            })
        }
    }

    /// Makes sure all terms in `sequence` are equal to each other, otherwise returns an `Expected`
    /// error.
    pub fn assert_all_eq(sequence: &[&Term]) -> Result<(), Self> {
        for i in 1..sequence.len() {
            Self::assert_eq(sequence[i - 1], sequence[i])?;
        }
        Ok(())
    }

    /// Returns an `ExpectedOneOf` sort error if `got` is not in `possibilities`.
    pub fn assert_one_of(possibilities: &[&Term], got: &Term) -> Result<(), Self> {
        match possibilities.iter().find(|&&s| s == got) {
            Some(_) => Ok(()),
            None => Err(Self::ExpectedOneOf {
                possibilities: possibilities.iter().map(|t| (*t).clone()).collect(),
                got: got.clone(),
            }),
        }
    }
}
