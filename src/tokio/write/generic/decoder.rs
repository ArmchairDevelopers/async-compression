use core::{
    pin::Pin,
    task::{Context, Poll},
};
use std::io::{Error, ErrorKind, Result};

use crate::{
    codec::Decode,
    tokio::write::{AsyncBufWrite, BufWriter},
    util::PartialBuffer,
};
use futures_core::ready;
use pin_project_lite::pin_project;
use tokio::io::AsyncWrite;

#[derive(Debug)]
enum State {
    Decoding,
    Finishing,
    Done,
}

pin_project! {
    #[derive(Debug)]
    pub struct Decoder<W, D: Decode> {
        #[pin]
        writer: BufWriter<W>,
        decoder: D,
        state: State,
    }
}

impl<W: AsyncWrite, D: Decode> Decoder<W, D> {
    pub fn new(writer: W, decoder: D) -> Self {
        Self {
            writer: BufWriter::new(writer),
            decoder,
            state: State::Decoding,
        }
    }

    pub fn get_ref(&self) -> &W {
        self.writer.get_ref()
    }

    pub fn get_mut(&mut self) -> &mut W {
        self.writer.get_mut()
    }

    pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut W> {
        self.project().writer.get_pin_mut()
    }

    pub fn into_inner(self) -> W {
        self.writer.into_inner()
    }

    pub fn decoder_mut(&mut self) -> &mut D {
        &mut self.decoder
    }

    fn do_poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        input: &mut PartialBuffer<&[u8]>,
    ) -> Poll<Result<()>> {
        let mut this = self.project();

        loop {
            let output = ready!(this.writer.as_mut().poll_partial_flush_buf(cx))?;
            let mut output = PartialBuffer::new(output);

            *this.state = match this.state {
                State::Decoding => {
                    if this.decoder.decode(input, &mut output)? {
                        State::Finishing
                    } else {
                        State::Decoding
                    }
                }

                State::Finishing => {
                    if this.decoder.finish(&mut output)? {
                        State::Done
                    } else {
                        State::Finishing
                    }
                }

                State::Done => {
                    return Poll::Ready(Err(Error::new(
                        ErrorKind::Other,
                        "Write after end of stream",
                    )))
                }
            };

            let produced = output.written().len();
            this.writer.as_mut().produce(produced);

            if let State::Done = this.state {
                return Poll::Ready(Ok(()));
            }

            if input.unwritten().is_empty() {
                return Poll::Ready(Ok(()));
            }
        }
    }

    fn do_poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let mut this = self.project();

        loop {
            let output = ready!(this.writer.as_mut().poll_partial_flush_buf(cx))?;
            let mut output = PartialBuffer::new(output);

            let (state, done) = match this.state {
                State::Decoding => {
                    let done = this.decoder.flush(&mut output)?;
                    (State::Decoding, done)
                }

                State::Finishing => {
                    if this.decoder.finish(&mut output)? {
                        (State::Done, false)
                    } else {
                        (State::Finishing, false)
                    }
                }

                State::Done => (State::Done, true),
            };

            *this.state = state;

            let produced = output.written().len();
            this.writer.as_mut().produce(produced);

            if done {
                return Poll::Ready(Ok(()));
            }
        }
    }
}

impl<W: AsyncWrite, D: Decode> AsyncWrite for Decoder<W, D> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let mut input = PartialBuffer::new(buf);

        match self.do_poll_write(cx, &mut input)? {
            Poll::Pending if input.written().is_empty() => Poll::Pending,
            _ => Poll::Ready(Ok(input.written().len())),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        ready!(self.as_mut().do_poll_flush(cx))?;
        ready!(self.project().writer.as_mut().poll_flush(cx))?;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        if let State::Decoding = self.as_mut().project().state {
            *self.as_mut().project().state = State::Finishing;
        }

        ready!(self.as_mut().do_poll_flush(cx))?;

        if let State::Done = self.as_mut().project().state {
            ready!(self.as_mut().project().writer.as_mut().poll_shutdown(cx))?;
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(Error::new(
                ErrorKind::Other,
                "Attempt to shutdown before finishing input",
            )))
        }
    }
}
