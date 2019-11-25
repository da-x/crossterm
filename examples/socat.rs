//
// time socat -s -b4096 -u OPEN:./test.txt,ignoreeof EXEC:'cargo run --example socat',pty,setsid,ctty
//
use std::time::Duration;

use crossterm::{
    event::{poll, read, Event, KeyCode},
    screen::RawScreen,
    Result,
};

const HELP: &str = r#"Blocking poll() & non-blocking read() & just wait :)
"#;

fn print_events() -> Result<usize> {
    let mut count: usize = 0;
    let mut none_count: usize = 0;
    let mut too_long_count: usize = 0;
    let start = std::time::Instant::now();

    loop {
        let start_poll = std::time::Instant::now();
        let poll_result = poll(Duration::from_secs(0))?;
        if start_poll.elapsed().as_millis() > 5 {
            too_long_count += 1;
        }

        if poll_result {
            let event = read()?;

            count += 1;

            if count % 1_000_000 == 0 {
                println!(
                    "Count: {} None count: {} Elapsed: {:?} Too long processing: {}",
                    count,
                    none_count,
                    start.elapsed(),
                    too_long_count
                );
            }

            if event == Event::Key(KeyCode::Char('Č').into()) {
                break;
            }
        } else {
            none_count += 1;
        }
    }

    println!(
        "Count: {} None count: {} Elapsed: {:?} Too long processing: {}",
        count,
        none_count,
        start.elapsed(),
        too_long_count
    );

    Ok(count)
}

fn main() -> Result<()> {
    println!("{}", HELP);

    let _r = RawScreen::into_raw_mode()?;

    if let Err(e) = print_events() {
        println!("Error: {:?}\r", e);
    }

    Ok(())
}
