// Lock-free ring buffer wrappers for inter-thread audio sample exchange.

use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapRb,
};

/// Single-producer single-consumer ring buffer for f32 audio samples.
pub struct AudioRingBuf {
    prod: ringbuf::HeapProd<f32>,
    cons: ringbuf::HeapCons<f32>,
}

impl AudioRingBuf {
    /// Create a new ring buffer with the given capacity in samples.
    pub fn new(capacity: usize) -> Self {
        let rb = HeapRb::<f32>::new(capacity);
        let (prod, cons) = rb.split();
        Self { prod, cons }
    }

    /// Split into producer and consumer halves for use in separate threads.
    pub fn split(self) -> (AudioProducer, AudioConsumer) {
        (
            AudioProducer { inner: self.prod },
            AudioConsumer { inner: self.cons },
        )
    }
}

/// Producer half: push audio samples from a capture thread.
pub struct AudioProducer {
    inner: ringbuf::HeapProd<f32>,
}

impl AudioProducer {
    /// Push a slice of samples. Returns number of samples actually written.
    pub fn push(&mut self, data: &[f32]) -> usize {
        self.inner.push_slice(data)
    }

    /// Number of samples waiting to be consumed.
    pub fn available(&self) -> usize {
        self.inner.occupied_len()
    }
}

// SAFETY: The ring buffer producer is only used from one thread.
unsafe impl Send for AudioProducer {}

/// Consumer half: pull audio samples from a processing thread.
pub struct AudioConsumer {
    inner: ringbuf::HeapCons<f32>,
}

impl AudioConsumer {
    /// Pop up to `dst.len()` samples into dst. Returns number of samples read.
    pub fn pop(&mut self, dst: &mut [f32]) -> usize {
        self.inner.pop_slice(dst)
    }

    /// Number of samples available for reading.
    pub fn available(&self) -> usize {
        self.inner.occupied_len()
    }
}

// SAFETY: The ring buffer consumer is only used from one thread.
unsafe impl Send for AudioConsumer {}
