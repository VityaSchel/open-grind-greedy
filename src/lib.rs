pub mod bot;
pub mod embedder;
pub mod embedding_cache;
pub mod faq;
pub mod matcher;

pub use embedder::FastembedEmbedder;
pub use embedding_cache::EmbeddingCache;
pub use faq::{FaqEntry, load_faq};
pub use matcher::{EmbedReport, Embedder, Match, Matcher};
