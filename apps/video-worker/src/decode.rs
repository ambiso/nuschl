// SPDX-License-Identifier: EUPL-1.2

mod color;
#[cfg(feature = "vaapi")]
mod vaapi;

use nuschl_ipc::video::{
    DecodedFrame, HEADER_BYTES, Header, Kind, Source, decode_packet_limit, source_from_byte,
};
use rav1d::{Decoder, Picture, Rav1dError, Settings};
use std::io::{self, Read, Write};

type Error = Box<dyn std::error::Error>;

pub fn run(accelerated: bool) -> Result<(), Error> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut header = [0; HEADER_BYTES];
    input.read_exact(&mut header)?;
    let configure = Header {
        bytes: 1,
        ..Header::empty(Kind::Configure, 0)
    };
    if Header::from_bytes(header)? != configure {
        return Err("expected video decoder configuration".into());
    }
    let mut source = [0];
    input.read_exact(&mut source)?;
    let source = source_from_byte(source[0])?;

    let mut backend = if accelerated {
        #[cfg(feature = "vaapi")]
        if let Ok(pipeline) = vaapi::Pipeline::start() {
            Some(Backend::Hardware(pipeline))
        } else {
            None
        }
        #[cfg(not(feature = "vaapi"))]
        None
    } else {
        Some(Backend::Software(software(source)?))
    };
    let Some(backend) = &mut backend else {
        return reply(&mut output, Header::empty(Kind::Dropped, 0));
    };
    reply(&mut output, Header::empty(Kind::Ready, 0))?;

    let mut previous = 0;
    let mut needs_key = true;
    while input.read(&mut header[..1])? != 0 {
        input.read_exact(&mut header[1..])?;
        let request = Header::from_bytes(header)?;
        if request == Header::empty(Kind::ResetDecoder, 0) {
            if previous != 0 {
                backend.reset()?;
            }
            needs_key = true;
            continue;
        }
        if request.kind != Kind::Decode
            || request.id <= previous
            || request.bytes == 0
            || request.bytes as usize > decode_packet_limit(source, request.key)
            || (needs_key && !request.key)
        {
            return Err("invalid video decoder request".into());
        }
        previous = request.id;
        needs_key = false;
        let mut encoded = vec![0; request.bytes as usize];
        input.read_exact(&mut encoded)?;
        let Some(frame) = backend.decode(encoded, request.id, source)? else {
            reply(&mut output, Header::empty(Kind::Dropped, request.id))?;
            continue;
        };
        let decoded = Header {
            bytes: u32::try_from(frame.bgra.len() + 4)?,
            ..Header::empty(Kind::Decoded, request.id)
        };
        output.write_all(&decoded.to_bytes())?;
        output.write_all(&frame.width.to_be_bytes())?;
        output.write_all(&frame.height.to_be_bytes())?;
        output.write_all(&frame.bgra)?;
        output.flush()?;
    }
    Ok(())
}

fn reply(output: &mut impl Write, header: Header) -> Result<(), Error> {
    output.write_all(&header.to_bytes())?;
    output.flush()?;
    Ok(())
}

enum Backend {
    Software(Decoder),
    #[cfg(feature = "vaapi")]
    Hardware(vaapi::Pipeline),
}

impl Backend {
    fn reset(&mut self) -> Result<(), Error> {
        match self {
            Self::Software(decoder) => decoder.flush(),
            #[cfg(feature = "vaapi")]
            Self::Hardware(decoder) => decoder.reset()?,
        }
        Ok(())
    }

    fn decode(
        &mut self,
        encoded: Vec<u8>,
        id: u64,
        source: Source,
    ) -> Result<Option<DecodedFrame>, Error> {
        match self {
            Self::Software(decoder) => software_picture(decoder, encoded, i64::try_from(id)?)?
                .map(|picture| color::to_bgra(&picture, source))
                .transpose(),
            #[cfg(feature = "vaapi")]
            Self::Hardware(decoder) => decoder.decode(encoded, id, source),
        }
    }
}

fn software(source: Source) -> Result<Decoder, Error> {
    let mut settings = Settings::new();
    settings.set_n_threads(4);
    settings.set_max_frame_delay(1);
    settings.set_frame_size_limit(match source {
        Source::Camera => 1920 * 1080,
        Source::Screen => 3840 * 2160,
    });
    settings.set_all_layers(false);
    Ok(Decoder::with_settings(&settings)?)
}

fn software_picture(
    decoder: &mut Decoder,
    encoded: Vec<u8>,
    id: i64,
) -> Result<Option<Picture>, Error> {
    let mut send = decoder.send_data(encoded.into_boxed_slice(), None, Some(id), None);
    let mut current = None;
    // An input can contain several coded frames. Display only the newest
    // picture for this temporal unit and bound the work on its output queue.
    for _ in 0..32 {
        let pending = match send {
            Ok(()) => false,
            Err(Rav1dError::TryAgain) => true,
            Err(error) => return Err(error.into()),
        };
        match decoder.get_picture() {
            Ok(picture) if picture.timestamp() == Some(id) => current = Some(picture),
            Ok(_) => {}
            Err(Rav1dError::TryAgain) if !pending => return Ok(current),
            Err(error) => return Err(error.into()),
        }
        if pending {
            send = decoder.send_pending_data();
        }
    }
    Err("too many pictures in one AV1 temporal unit".into())
}
