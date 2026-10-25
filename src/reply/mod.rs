mod format;
mod mail;
mod worker;

pub use format::{Rejection, Relation, ReplyFormat, Verdict};
pub use mail::{answer, build_email, MailTransport, SendError, SendFuture, SmtpTransport};
pub use worker::{FeedbackFuture, ReplyFeedback, ReplyWorker, ReplyWorkerConfig};
