use xtra::{Actor, Handler, Message};

struct Printer {
    times: usize,
}

impl Printer {
    fn new() -> Self {
        Printer { times: 0 }
    }
}

impl Actor for Printer {}

struct Print(String);
impl Message for Print {
    type Result = ();
}

impl Handler<Print> for Printer {
    fn handle(&mut self, print: Print) {
        self.times += 1;
        println!("Printing {}. Printed {} times so far.", print.0, self.times);
    }
}

#[tokio::main]
async fn main() {
    let addr = Printer::new().spawn();
    loop {
        addr.send(Print("hello".to_string())).await;
    }
}