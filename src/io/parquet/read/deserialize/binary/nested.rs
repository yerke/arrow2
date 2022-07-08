use std::collections::VecDeque;

use parquet2::{
    encoding::Encoding,
    page::{split_buffer, DataPage},
    schema::Repetition,
};

use crate::{
    array::Offset, bitmap::MutableBitmap, datatypes::DataType, error::Result,
    io::parquet::read::DataPages,
};

use super::super::nested_utils::*;
use super::super::utils::MaybeNext;
use super::basic::ValuesDictionary;
use super::utils::*;
use super::{
    super::utils,
    basic::{finish, TraitBinaryArray},
};

#[derive(Debug)]
enum State<'a> {
    Optional(BinaryIter<'a>),
    Required(BinaryIter<'a>),
    RequiredDictionary(ValuesDictionary<'a>),
    OptionalDictionary(ValuesDictionary<'a>),
}

impl<'a> utils::PageState<'a> for State<'a> {
    fn len(&self) -> usize {
        match self {
            State::Optional(validity) => validity.size_hint().0,
            State::Required(state) => state.size_hint().0,
            State::RequiredDictionary(required) => required.len(),
            State::OptionalDictionary(optional) => optional.len(),
        }
    }
}

#[derive(Debug, Default)]
struct BinaryDecoder<O: Offset> {
    phantom_o: std::marker::PhantomData<O>,
}

impl<'a, O: Offset> NestedDecoder<'a> for BinaryDecoder<O> {
    type State = State<'a>;
    type DecodedState = (Binary<O>, MutableBitmap);

    fn build_state(&self, page: &'a DataPage) -> Result<Self::State> {
        let is_optional =
            page.descriptor.primitive_type.field_info.repetition == Repetition::Optional;
        let is_filtered = page.selected_rows().is_some();

        match (
            page.encoding(),
            page.dictionary_page(),
            is_optional,
            is_filtered,
        ) {
            (Encoding::PlainDictionary | Encoding::RleDictionary, Some(dict), false, false) => {
                let dict = dict.as_any().downcast_ref().unwrap();
                ValuesDictionary::try_new(page, dict).map(State::RequiredDictionary)
            }
            (Encoding::PlainDictionary | Encoding::RleDictionary, Some(dict), true, false) => {
                let dict = dict.as_any().downcast_ref().unwrap();
                ValuesDictionary::try_new(page, dict).map(State::OptionalDictionary)
            }
            (Encoding::Plain, _, true, false) => {
                let (_, _, values) = split_buffer(page)?;

                let values = BinaryIter::new(values);

                Ok(State::Optional(values))
            }
            (Encoding::Plain, _, false, false) => {
                let (_, _, values) = split_buffer(page)?;

                let values = BinaryIter::new(values);

                Ok(State::Required(values))
            }
            _ => Err(utils::not_implemented(page)),
        }
    }

    fn with_capacity(&self, capacity: usize) -> Self::DecodedState {
        (
            Binary::<O>::with_capacity(capacity),
            MutableBitmap::with_capacity(capacity),
        )
    }

    fn push_valid(&self, state: &mut Self::State, decoded: &mut Self::DecodedState) {
        let (values, validity) = decoded;
        match state {
            State::Optional(page) => {
                let value = page.next().unwrap_or_default();
                values.push(value);
                validity.push(true);
            }
            State::Required(page) => {
                let value = page.next().unwrap_or_default();
                values.push(value);
            }
            State::RequiredDictionary(page) => {
                let dict_values = page.dict.values();
                let dict_offsets = page.dict.offsets();

                let op = move |index: u32| {
                    let index = index as usize;
                    let dict_offset_i = dict_offsets[index] as usize;
                    let dict_offset_ip1 = dict_offsets[index + 1] as usize;
                    &dict_values[dict_offset_i..dict_offset_ip1]
                };
                let item = page.values.next().map(op).unwrap_or_default();
                values.push(item);
            }
            State::OptionalDictionary(page) => {
                let dict_values = page.dict.values();
                let dict_offsets = page.dict.offsets();

                let op = move |index: u32| {
                    let index = index as usize;
                    let dict_offset_i = dict_offsets[index] as usize;
                    let dict_offset_ip1 = dict_offsets[index + 1] as usize;
                    &dict_values[dict_offset_i..dict_offset_ip1]
                };
                let item = page.values.next().map(op).unwrap_or_default();
                values.push(item);
                validity.push(true);
            }
        }
    }

    fn push_null(&self, decoded: &mut Self::DecodedState) {
        let (values, validity) = decoded;
        values.push(&[]);
        validity.push(false);
    }
}

pub struct ArrayIterator<O: Offset, A: TraitBinaryArray<O>, I: DataPages> {
    iter: I,
    data_type: DataType,
    init: Vec<InitNested>,
    items: VecDeque<(NestedState, (Binary<O>, MutableBitmap))>,
    chunk_size: Option<usize>,
    phantom_a: std::marker::PhantomData<A>,
}

impl<O: Offset, A: TraitBinaryArray<O>, I: DataPages> ArrayIterator<O, A, I> {
    pub fn new(
        iter: I,
        init: Vec<InitNested>,
        data_type: DataType,
        chunk_size: Option<usize>,
    ) -> Self {
        Self {
            iter,
            data_type,
            init,
            items: VecDeque::new(),
            chunk_size,
            phantom_a: Default::default(),
        }
    }
}

impl<O: Offset, A: TraitBinaryArray<O>, I: DataPages> Iterator for ArrayIterator<O, A, I> {
    type Item = Result<(NestedState, A)>;

    fn next(&mut self) -> Option<Self::Item> {
        let maybe_state = next(
            &mut self.iter,
            &mut self.items,
            &self.init,
            self.chunk_size,
            &BinaryDecoder::<O>::default(),
        );
        match maybe_state {
            MaybeNext::Some(Ok((nested, decoded))) => {
                Some(finish(&self.data_type, decoded.0, decoded.1).map(|array| (nested, array)))
            }
            MaybeNext::Some(Err(e)) => Some(Err(e)),
            MaybeNext::None => None,
            MaybeNext::More => self.next(),
        }
    }
}