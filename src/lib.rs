pub mod bot;
pub mod embedder;
pub mod faq;
pub mod matcher;

pub use embedder::FastembedEmbedder;
pub use faq::{FaqEntry, load_faq};
pub use matcher::{Embedder, Match, Matcher};
