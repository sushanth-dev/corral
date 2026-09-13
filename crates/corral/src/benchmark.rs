//! 30-pane renderer benchmark (plan Task 8).
//!
//! Ignored by default; run with:
//! `cargo test -p corral --release -- benchmark -- --ignored --nocapture`
//!
//! Lives in `src/` rather than `benches/` because a bench target cannot
//! import the bin crate's `render` module.

#[cfg(test)]
mod bench {
    use std::time::Instant;

    use corral_core::emulation::Emulator;
    use corral_core::tree::{Node, Rect};
    use corrald::protocol::PaneState;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::render;

    const PANES: usize = 30;
    const LINES_PER_PANE: usize = 500;
    const DRAWS: usize = 100;
    const WIDTH: u16 = 300;
    const HEIGHT: u16 = 80;

    fn balanced_tree(ids: &[u32]) -> Node {
        match ids {
            [] => unreachable!("caller guarantees a nonempty slice"),
            [id] => Node::leaf(*id),
            _ => {
                let mid = ids.len() / 2;
                Node::split(
                    corral_core::tree::Dir::Vertical,
                    0.5,
                    Box::new(balanced_tree(&ids[..mid])),
                    Box::new(balanced_tree(&ids[mid..])),
                )
            }
        }
    }

    #[test]
    #[ignore = "measurement harness; run with --ignored"]
    fn benchmark_thirty_pane_frames() {
        let area = Rect {
            x: 0,
            y: 0,
            w: WIDTH,
            h: HEIGHT,
        };

        let ids: Vec<u32> = (0..PANES as u32).collect();
        let tree = balanced_tree(&ids);
        let rects = tree.rects(area);
        assert_eq!(rects.len(), PANES, "tree must tile into {PANES} panes");

        let mut emulators: Vec<Emulator> = Vec::new();
        for _ in 0..PANES {
            let mut emu = Emulator::new(80, 24).expect("emulator");
            let body: String = (0..LINES_PER_PANE)
                .map(|i| format!("pane line {i}\r\n"))
                .collect();
            emu.feed(body.as_bytes());
            emulators.push(emu);
        }

        let mut panes: Vec<PaneState> = Vec::new();
        for (id, rect) in rects {
            let text = emulators[id as usize].screen_text().expect("screen text");
            panes.push(PaneState {
                id,
                rect,
                text,
                cursor: None,
                app_cursor: false,
                scroll: None,
            });
        }

        let backend = TestBackend::new(WIDTH, HEIGHT);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let focused = 0;

        let mut times: Vec<u128> = Vec::with_capacity(DRAWS);
        for _ in 0..DRAWS {
            let start = Instant::now();
            for pane in &mut panes {
                pane.text = emulators[pane.id as usize]
                    .screen_text()
                    .expect("screen text");
            }
            let snapshot = panes.clone();
            terminal
                .draw(|frame| render::draw(frame, &snapshot, focused, render::Hint::None, &[]))
                .expect("draw");
            times.push(start.elapsed().as_millis());
        }

        let drew = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|c| !c.symbol().is_empty());
        assert!(drew, "frame buffer is empty; nothing drew");

        for p in &panes {
            let (id, rect) = (p.id, p.rect);
            let area_cells = rect.w as usize * rect.h as usize;
            let drawn = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .skip(rect.y as usize * WIDTH as usize + rect.x as usize)
                .take(area_cells)
                .filter(|c| !c.symbol().is_empty())
                .count();
            assert!(drawn > 0, "pane {id} at {rect:?} drew no cells");
        }

        times.sort_unstable();
        let min = times[0];
        let median = times[DRAWS / 2];
        let max = times[DRAWS - 1];
        let p95 = times[(DRAWS as f32 * 0.95) as usize];
        std::hint::black_box(&times);

        println!("30 panes, {LINES_PER_PANE} lines each, {DRAWS} draws at {WIDTH}x{HEIGHT}");
        println!("min {min} ms | median {median} ms | p95 {p95} ms | max {max} ms");
    }
}
