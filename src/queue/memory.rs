use crate::error::{ChimneyError, Result};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct Message {
    pub from: Option<String>,
    pub to: Vec<String>,
    pub subject: Option<String>,
    pub body: String,
}

#[derive(Clone)]
pub struct MessageQueue {
    sender: mpsc::Sender<Message>,
}

pub struct MessageReceiver {
    receiver: mpsc::Receiver<Message>,
}

impl MessageQueue {
    pub fn new(capacity: usize) -> (Self, MessageReceiver) {
        let (sender, receiver) = mpsc::channel(capacity);
        (Self { sender }, MessageReceiver { receiver })
    }

    pub async fn send(&self, message: Message) -> Result<()> {
        self.sender
            .send(message)
            .await
            .map_err(|_| ChimneyError::Smtp("message queue closed".to_string()))
    }
}

impl MessageReceiver {
    pub async fn recv(&mut self) -> Option<Message> {
        self.receiver.recv().await
    }
}
