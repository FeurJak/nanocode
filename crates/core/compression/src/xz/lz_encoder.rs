mod lzma2_encoder;
mod lzma_encoder;
mod match_finder;
mod optimum_fast;
mod optimum_normal;
mod price;
mod range_encoder;

use alloc::boxed::Box;

use super::coder::{Code, Coder};
use super::lzma_common::{REPS, State};
use super::options::LzmaOptions;
use super::stream::{Action, Error, Status};
use lzma_encoder::LzmaEncoder;
use lzma2_encoder::Lzma2Encoder;
use match_finder::MatchFinder;

enum Payload {
    Lzma1(Box<LzmaEncoder>),
    Lzma2(Box<Lzma2Encoder>),
}

#[derive(Clone)]
pub struct LzmaState {
    probabilities: Box<[u16; lzma_encoder::PROBABILITIES]>,
    state: State,
    reps: [u32; REPS],
    position: u64,
    reset: bool,
    properties: bool,
}

impl LzmaState {
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.position
    }
}

struct LzEncoder {
    mf: MatchFinder,
    payload: Payload,
    next: Option<Coder>,
}

pub(crate) fn lzma1_encoder(options: &LzmaOptions, next: Option<Coder>) -> Result<Coder, Error> {
    let (encoder, settings) = LzmaEncoder::new(options, true)?;
    let mf = MatchFinder::new(&settings)?;
    Ok(Box::new(LzEncoder {
        mf,
        payload: Payload::Lzma1(encoder),
        next,
    }))
}

pub(crate) fn lzma2_encoder(options: &LzmaOptions, next: Option<Coder>) -> Result<Coder, Error> {
    let (encoder, settings) = Lzma2Encoder::new(options)?;
    let mf = MatchFinder::new(&settings)?;
    Ok(Box::new(LzEncoder {
        mf,
        payload: Payload::Lzma2(encoder),
        next,
    }))
}

pub(crate) fn lzma2_resumed(
    options: &LzmaOptions,
    window: &[u8],
    position: u64,
    state: Option<&LzmaState>,
) -> Result<Coder, Error> {
    let (mut encoder, settings) = Lzma2Encoder::new(options)?;
    let mut mf = MatchFinder::new(&settings)?;
    mf.prime(window);
    encoder.resume(position, mf.read_pos > 0, state);
    Ok(Box::new(LzEncoder {
        mf,
        payload: Payload::Lzma2(encoder),
        next: None,
    }))
}

impl Code for LzEncoder {
    fn lzma_state(&self) -> Option<LzmaState> {
        match &self.payload {
            Payload::Lzma2(encoder) if self.next.is_none() => encoder.snapshot(&self.mf),
            Payload::Lzma1(_) | Payload::Lzma2(_) => None,
        }
    }

    fn code(
        &mut self,
        input: &[u8],
        input_position: &mut usize,
        output: &mut [u8],
        output_position: &mut usize,
        action: Action,
    ) -> Result<Status, Error> {
        while *output_position < output.len()
            && (*input_position < input.len() || action != Action::Run)
        {
            if self.mf.action == Action::Run && self.mf.read_pos >= self.mf.read_limit {
                let status =
                    self.mf
                        .fill_window(self.next.as_mut(), input, input_position, action)?;
                if status != Status::Ok {
                    return Ok(status);
                }
            }
            let status = match &mut self.payload {
                Payload::Lzma1(encoder) => {
                    encoder.encode(&mut self.mf, output, output_position, None)
                }
                Payload::Lzma2(encoder) => encoder.encode(&mut self.mf, output, output_position),
            };
            if status != Status::Ok {
                self.mf.action = Action::Run;
                return Ok(status);
            }
        }
        Ok(Status::Ok)
    }
}
