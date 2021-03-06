//! Interpretation of DICOM data sets as streams of tokens.
use dicom_core::header::{DataElementHeader, Length, VR};
use dicom_core::value::{DicomValueType, PrimitiveValue};
use dicom_core::{value::Value, DataElement, Tag};
use std::fmt;

pub mod read;
pub mod write;

pub use self::read::DataSetReader;
pub use self::write::DataSetWriter;

/// A token of a DICOM data set stream. This is part of the interpretation of a
/// data set as a stream of symbols, which may either represent data headers or
/// actual value data.
#[derive(Debug, Clone)]
pub enum DataToken {
    /// A data header of a primitive value.
    ElementHeader(DataElementHeader),
    /// The beginning of a sequence element.
    SequenceStart { tag: Tag, len: Length },
    /// The beginning of an encapsulated pixel data element.
    PixelSequenceStart,
    /// The ending delimiter of a sequence or encapsulated pixel data.
    SequenceEnd,
    /// The beginning of a new item in the sequence.
    ItemStart { len: Length },
    /// The ending delimiter of an item.
    ItemEnd,
    /// A primitive data element value.
    PrimitiveValue(PrimitiveValue),
    /// An owned piece of raw data representing an item's value.
    ///
    /// This variant is used to represent the value of an offset table or a
    /// compressed fragment. It should not be used to represent nested data
    /// sets.
    ItemValue(Vec<u8>),
}

impl fmt::Display for DataToken {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            DataToken::PrimitiveValue(ref v) => write!(f, "PrimitiveValue({:?})", v.value_type()),
            other => write!(f, "{:?}", other),
        }
    }
}

/// This implementation treats undefined lengths as equal.
impl PartialEq<Self> for DataToken {
    fn eq(&self, other: &Self) -> bool {
        use DataToken::*;
        match (self, other) {
            (
                ElementHeader(DataElementHeader {
                    tag: tag1,
                    vr: vr1,
                    len: len1,
                }),
                ElementHeader(DataElementHeader {
                    tag: tag2,
                    vr: vr2,
                    len: len2,
                }),
            ) => tag1 == tag2 && vr1 == vr2 && len1.inner_eq(*len2),
            (
                SequenceStart {
                    tag: tag1,
                    len: len1,
                },
                SequenceStart {
                    tag: tag2,
                    len: len2,
                },
            ) => tag1 == tag2 && len1.inner_eq(*len2),
            (ItemStart { len: len1 }, ItemStart { len: len2 }) => len1.inner_eq(*len2),
            (PrimitiveValue(v1), PrimitiveValue(v2)) => v1 == v2,
            (ItemValue(v1), ItemValue(v2)) => v1 == v2,
            (ItemEnd, ItemEnd)
            | (SequenceEnd, SequenceEnd)
            | (PixelSequenceStart, PixelSequenceStart) => true,
            _ => false,
        }
    }
}

impl From<DataElementHeader> for DataToken {
    fn from(header: DataElementHeader) -> Self {
        match (header.vr(), header.tag) {
            (VR::OB, Tag(0x7fe0, 0x0010)) if header.len.is_undefined() => {
                DataToken::PixelSequenceStart
            }
            (VR::SQ, _) => DataToken::SequenceStart {
                tag: header.tag,
                len: header.len,
            },
            _ => DataToken::ElementHeader(header),
        }
    }
}

impl DataToken {
    /// Check whether this token represents the start of a sequence
    /// of nested data sets.
    pub fn is_sequence_start(&self) -> bool {
        match self {
            DataToken::SequenceStart { .. } => true,
            _ => false,
        }
    }

    /// Check whether this token represents the end of a sequence
    /// or the end of an encapsulated element.
    pub fn is_sequence_end(&self) -> bool {
        match self {
            DataToken::SequenceEnd => true,
            _ => false,
        }
    }
}

/// The type of delimiter: sequence or item.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SeqTokenType {
    Sequence,
    Item,
}

/// A trait for converting structured DICOM data into a stream of data tokens.
pub trait IntoTokens {
    /// The iterator type through which tokens are obtained.
    type Iter: Iterator<Item = DataToken>;

    /// Convert the value into tokens.
    fn into_tokens(self) -> Self::Iter;
}

impl IntoTokens for dicom_core::header::EmptyObject {
    type Iter = std::iter::Empty<DataToken>;

    fn into_tokens(self) -> Self::Iter {
        unreachable!()
    }
}

/// Token generator from a DICOM data element.
pub enum DataElementTokens<I, P>
where
    I: IntoTokens,
{
    /// initial state, at the beginning of the element
    Start(
        // Option is used for easy taking from a &mut,
        // should always be Some in practice
        Option<DataElement<I, P>>,
    ),
    /// the header of a plain primitive element was read
    Header(
        // Option is used for easy taking from a &mut,
        // should always be Some in practice
        Option<DataElement<I, P>>,
    ),
    /// reading tokens from items
    Items(
        FlattenTokens<
            <dicom_core::value::C<AsItem<I>> as IntoIterator>::IntoIter,
            ItemTokens<I::Iter>,
        >,
    ),
    /// the header of encapsulated pixel data was read, will read
    /// the offset table next
    PixelData(
        // Pixel fragments; Option is used for easy taking from a &mut,
        // should always be Some in practice
        Option<dicom_core::value::C<P>>,
        ItemValueTokens<dicom_core::value::C<u8>>,
    ),
    /// the header and offset of encapsulated pixel data was read,
    /// fragments come next
    PixelDataFragments(
        FlattenTokens<
            <dicom_core::value::C<ItemValue<P>> as IntoIterator>::IntoIter,
            ItemValueTokens<P>,
        >,
    ),
    /// no more elements
    End,
}

impl<I, P> Iterator for DataElementTokens<I, P>
where
    I: IntoTokens,
    P: AsRef<[u8]>,
{
    type Item = DataToken;

    fn next(&mut self) -> Option<Self::Item> {
        let (out, next_state) = match self {
            DataElementTokens::Start(elem) => {
                let elem = elem.take().unwrap();
                // data element header token
                let header = *elem.header();

                let token = DataToken::from(header);
                match token {
                    DataToken::SequenceStart { .. } => {
                        // retrieve sequence value, begin item sequence
                        match elem.into_value() {
                            Value::Primitive(_) | Value::PixelSequence { .. } => unreachable!(),
                            Value::Sequence { items, size: _ } => {
                                let items: dicom_core::value::C<_> = items
                                    .into_iter()
                                    .map(|o| AsItem(Length::UNDEFINED, o))
                                    .collect();
                                (Some(token), DataElementTokens::Items(items.into_tokens()))
                            }
                        }
                    }
                    DataToken::PixelSequenceStart => {
                        match elem.into_value() {
                            Value::PixelSequence {
                                fragments,
                                offset_table,
                            } => {
                                (
                                    // begin pixel sequence
                                    Some(DataToken::PixelSequenceStart),
                                    DataElementTokens::PixelData(
                                        Some(fragments),
                                        ItemValue(offset_table).into_tokens(),
                                    ),
                                )
                            }
                            Value::Primitive(_) | Value::Sequence { .. } => unreachable!(),
                        }
                    }
                    _ => (
                        Some(DataToken::ElementHeader(*elem.header())),
                        DataElementTokens::Header(Some(elem)),
                    ),
                }
            }
            DataElementTokens::Header(elem) => {
                let elem = elem.take().unwrap();
                match elem.into_value() {
                    Value::Sequence { .. } | Value::PixelSequence { .. } => unreachable!(),
                    Value::Primitive(value) => {
                        // return primitive value, done
                        let token = DataToken::PrimitiveValue(value);
                        (Some(token), DataElementTokens::End)
                    }
                }
            }
            DataElementTokens::Items(tokens) => {
                if let Some(token) = tokens.next() {
                    // bypass manual state transition
                    return Some(token);
                } else {
                    // sequence end token, end
                    (Some(DataToken::SequenceEnd), DataElementTokens::End)
                }
            }
            DataElementTokens::PixelData(fragments, tokens) => {
                if let Some(token) = tokens.next() {
                    // bypass manual state transition
                    return Some(token);
                }
                // pixel data fragments next
                let fragments = fragments.take().unwrap();
                let tokens: dicom_core::value::C<_> =
                    fragments.into_iter().map(|o| ItemValue(o)).collect();
                *self = DataElementTokens::PixelDataFragments(tokens.into_tokens());
                // recursive call to ensure the retrieval of a data token
                return self.next();
            }
            DataElementTokens::PixelDataFragments(tokens) => {
                if let Some(token) = tokens.next() {
                    // bypass manual state transition
                    return Some(token);
                } else {
                    // sequence end token, end
                    (Some(DataToken::SequenceEnd), DataElementTokens::End)
                }
            }
            DataElementTokens::End => return None,
        };
        *self = next_state;

        out
    }
}

impl<I, P> IntoTokens for DataElement<I, P>
where
    I: IntoTokens,
    P: AsRef<[u8]>,
{
    type Iter = DataElementTokens<I, P>;

    fn into_tokens(self) -> Self::Iter {
        DataElementTokens::Start(Some(self))
    }
}

/// Flatten a sequence of elements into their respective
/// token sequence in order.
#[derive(Debug, PartialEq)]
pub struct FlattenTokens<O, K> {
    seq: O,
    tokens: Option<K>,
}

impl<O, K> Iterator for FlattenTokens<O, K>
where
    O: Iterator,
    O::Item: IntoTokens<Iter = K>,
    K: Iterator<Item = DataToken>,
{
    type Item = DataToken;

    fn next(&mut self) -> Option<Self::Item> {
        // ensure a token sequence
        if self.tokens.is_none() {
            match self.seq.next() {
                Some(entries) => {
                    self.tokens = Some(entries.into_tokens());
                }
                None => return None,
            }
        }

        // retrieve the next token
        match self.tokens.as_mut().map(|s| s.next()) {
            Some(Some(token)) => Some(token),
            Some(None) => {
                self.tokens = None;
                self.next()
            }
            None => unreachable!(),
        }
    }
}

impl<T> IntoTokens for Vec<T>
where
    T: IntoTokens,
{
    type Iter = FlattenTokens<<Vec<T> as IntoIterator>::IntoIter, <T as IntoTokens>::Iter>;

    fn into_tokens(self) -> Self::Iter {
        FlattenTokens {
            seq: self.into_iter(),
            tokens: None,
        }
    }
}

impl<T> IntoTokens for dicom_core::value::C<T>
where
    T: IntoTokens,
{
    type Iter =
        FlattenTokens<<dicom_core::value::C<T> as IntoIterator>::IntoIter, <T as IntoTokens>::Iter>;

    fn into_tokens(self) -> Self::Iter {
        FlattenTokens {
            seq: self.into_iter(),
            tokens: None,
        }
    }
}

// A stream of tokens from a DICOM item.
#[derive(Debug)]
pub enum ItemTokens<T> {
    /// Just started, an item header token will come next
    Start {
        len: Length,
        object_tokens: Option<T>,
    },
    /// Will return tokens from the inner object, then an end of item token
    /// when it ends
    Object { object_tokens: T },
    /// Just ended, no more tokens
    End,
}

impl<T> ItemTokens<T>
where
    T: Iterator<Item = DataToken>,
{
    pub fn new<O>(len: Length, object: O) -> Self
    where
        O: IntoTokens<Iter = T>,
    {
        ItemTokens::Start {
            len,
            object_tokens: Some(object.into_tokens()),
        }
    }
}

impl<T> Iterator for ItemTokens<T>
where
    T: Iterator<Item = DataToken>,
{
    type Item = DataToken;

    fn next(&mut self) -> Option<Self::Item> {
        let (next_state, out) = match self {
            ItemTokens::Start { len, object_tokens } => (
                ItemTokens::Object {
                    object_tokens: object_tokens.take().unwrap(),
                },
                Some(DataToken::ItemStart { len: *len }),
            ),
            ItemTokens::Object { object_tokens } => {
                if let Some(token) = object_tokens.next() {
                    return Some(token);
                } else {
                    (ItemTokens::End, Some(DataToken::ItemEnd))
                }
            }
            ItemTokens::End => {
                return None;
            }
        };

        *self = next_state;
        out
    }
}

/// A newtype for interpreting the given data as an item.
/// When converting a value of this type into tokens, the inner value's tokens
/// will be surrounded by an item start and an item delimiter.
#[derive(Debug, Clone, PartialEq)]
pub struct AsItem<I>(Length, I);

impl<I> IntoTokens for AsItem<I>
where
    I: IntoTokens,
{
    type Iter = ItemTokens<I::Iter>;

    fn into_tokens(self) -> Self::Iter {
        ItemTokens::new(self.0, self.1)
    }
}

/// A newtype for wrapping a piece of raw data into an item.
/// When converting a value of this type into tokens, the algorithm
/// will create an item start with an explicit length, followed by
/// an item value token, then an item delimiter.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemValue<P>(P);

impl<P> IntoTokens for ItemValue<P>
where
    P: AsRef<[u8]>,
{
    type Iter = ItemValueTokens<P>;

    fn into_tokens(self) -> Self::Iter {
        ItemValueTokens::new(self.0)
    }
}

#[derive(Debug)]
pub enum ItemValueTokens<P> {
    /// Just started, an item header token will come next
    Start(Option<P>),
    /// Will return a token of the value
    Value(P),
    /// Will return an end of item token
    Done,
    /// Just ended, no more tokens
    End,
}

impl<P> ItemValueTokens<P> {
    pub fn new(value: P) -> Self {
        ItemValueTokens::Start(Some(value))
    }
}

impl<P> Iterator for ItemValueTokens<P>
where
    P: AsRef<[u8]>,
{
    type Item = DataToken;

    fn next(&mut self) -> Option<Self::Item> {
        let (out, next_state) = match self {
            ItemValueTokens::Start(value) => {
                let value = value.take().unwrap();
                let len = Length(value.as_ref().len() as u32);

                (
                    Some(DataToken::ItemStart { len }),
                    if len == Length(0) {
                        ItemValueTokens::Done
                    } else {
                        ItemValueTokens::Value(value)
                    },
                )
            }
            ItemValueTokens::Value(value) => (
                Some(DataToken::ItemValue(value.as_ref().to_owned())),
                ItemValueTokens::Done,
            ),
            ItemValueTokens::Done => (Some(DataToken::ItemEnd), ItemValueTokens::End),
            ItemValueTokens::End => return None,
        };

        *self = next_state;
        out
    }
}
