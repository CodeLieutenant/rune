pub(crate) mod ordering {
    use core::cmp::Ordering;

    use musli_core::{Context, Decoder, Encoder};

    pub(crate) fn encode<E>(value: &Ordering, encoder: E) -> Result<(), E::Error>
    where
        E: Encoder,
    {
        match value {
            Ordering::Less => encoder.encode_i8(-1),
            Ordering::Equal => encoder.encode_i8(0),
            Ordering::Greater => encoder.encode_i8(1),
        }
    }

    pub(crate) fn decode<'de, D>(decoder: D) -> Result<Ordering, D::Error>
    where
        D: Decoder<'de>,
    {
        let cx = decoder.cx();

        match decoder.decode_i8()? {
            -1 => Ok(Ordering::Less),
            0 => Ok(Ordering::Equal),
            1 => Ok(Ordering::Greater),
            _ => Err(cx.message("invalid ordering")),
        }
    }
}

/// Encodes an optional width as one integer, zero for none. A packed layout
/// then stores the same number of values for `Some` and for `None`.
pub(crate) mod width {
    use core::num::NonZeroUsize;

    use musli_core::{Decoder, Encoder};

    pub(crate) fn encode<E>(value: &Option<NonZeroUsize>, encoder: E) -> Result<(), E::Error>
    where
        E: Encoder,
    {
        encoder.encode_usize(value.map_or(0, NonZeroUsize::get))
    }

    pub(crate) fn decode<'de, D>(decoder: D) -> Result<Option<NonZeroUsize>, D::Error>
    where
        D: Decoder<'de>,
    {
        Ok(NonZeroUsize::new(decoder.decode_usize()?))
    }
}

/// Encodes an optional count as one integer, zero for none and the count plus
/// one for a value.
pub(crate) mod count {
    use musli_core::{Context, Decoder, Encoder};

    pub(crate) fn encode<E>(value: &Option<usize>, encoder: E) -> Result<(), E::Error>
    where
        E: Encoder,
    {
        let cx = encoder.cx();

        let Some(value) = *value else {
            return encoder.encode_u64(0);
        };

        match u64::try_from(value).ok().and_then(|v| v.checked_add(1)) {
            Some(value) => encoder.encode_u64(value),
            None => Err(cx.message("count is too large to encode")),
        }
    }

    pub(crate) fn decode<'de, D>(decoder: D) -> Result<Option<usize>, D::Error>
    where
        D: Decoder<'de>,
    {
        let cx = decoder.cx();

        match decoder.decode_u64()? {
            0 => Ok(None),
            value => match usize::try_from(value - 1) {
                Ok(value) => Ok(Some(value)),
                Err(..) => Err(cx.message("count does not fit in usize")),
            },
        }
    }
}
