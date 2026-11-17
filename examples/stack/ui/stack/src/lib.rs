//! Stack: falling blocks on a canvas. It shows what a live view is made of: the plugin has no
//! memory and no clock, so the whole game is the state it returns and gets back, moved on by
//! the events the app sends (a tick, a key, a click on a canvas rectangle).

use crucible_view::{
    Context2d, Event, Key, Live, Node, Request, Stat, Style, TextAlign, Tone, View,
};

crucible_view::plugin!(inputs = [], view = stack);

const COLS: i32 = 10;
const ROWS: i32 = 18;
const CELL: f32 = 22.0;
const BOARD: f32 = COLS as f32 * CELL;
const SIDE: f32 = 170.0;
const HEIGHT: f32 = ROWS as f32 * CELL;

/// Each piece's four cells in its box before it is turned.
const SHAPES: [[(i32, i32); 4]; 7] = [
    [(0, 1), (1, 1), (2, 1), (3, 1)],
    [(1, 0), (2, 0), (1, 1), (2, 1)],
    [(1, 0), (0, 1), (1, 1), (2, 1)],
    [(1, 0), (2, 0), (0, 1), (1, 1)],
    [(0, 0), (1, 0), (1, 1), (2, 1)],
    [(0, 0), (0, 1), (1, 1), (2, 1)],
    [(2, 0), (0, 1), (1, 1), (2, 1)],
];

/// The side of the box each piece turns in.
const BOXES: [i32; 7] = [4, 2, 3, 3, 3, 3, 3];

const INKS: [Style; 7] = [
    Style::Tone(Tone::Info),
    Style::Tone(Tone::Warn),
    Style::Rgba([0xb0, 0x7c, 0xf7, 0xff]),
    Style::Tone(Tone::Good),
    Style::Tone(Tone::Bad),
    Style::Rgba([0x4f, 0xd1, 0xc5, 0xff]),
    Style::Rgba([0xf2, 0x9e, 0x4c, 0xff]),
];

/// Points for closing one to four rows at once.
const POINTS: [u32; 5] = [0, 100, 300, 500, 800];

#[derive(Clone, Debug, PartialEq)]
struct Game {
    /// A cell is 0 when empty, else one more than the piece that filled it.
    board: Vec<u8>,
    piece: usize,
    turn: i32,
    x: i32,
    y: i32,
    next: usize,
    seed: u32,
    score: u32,
    rows: u32,
    ticks: u32,
    over: bool,
}

/// The cells of `piece` after `turn` quarter turns, in its box.
fn cells(piece: usize, turn: i32) -> [(i32, i32); 4] {
    let side = BOXES[piece % 7];
    let mut out = SHAPES[piece % 7];
    if piece % 7 == 1 {
        return out;
    }
    for _ in 0..turn.rem_euclid(4) {
        for cell in &mut out {
            *cell = (side - 1 - cell.1, cell.0);
        }
    }
    out
}

fn roll(seed: &mut u32) -> usize {
    let mut x = (*seed).max(1);
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *seed = x;
    (x >> 8) as usize % 7
}

impl Game {
    fn new(seed: u32) -> Game {
        let mut seed = seed;
        let piece = roll(&mut seed);
        let next = roll(&mut seed);
        Game {
            board: vec![0; (COLS * ROWS) as usize],
            piece,
            turn: 0,
            x: 3,
            y: 0,
            next,
            seed,
            score: 0,
            rows: 0,
            ticks: 0,
            over: false,
        }
    }

    fn at(&self, x: i32, y: i32) -> Option<u8> {
        if (0..COLS).contains(&x) && (0..ROWS).contains(&y) {
            self.board.get((y * COLS + x) as usize).copied()
        } else {
            None
        }
    }

    fn fits(&self, turn: i32, x: i32, y: i32) -> bool {
        cells(self.piece, turn)
            .iter()
            .all(|(cx, cy)| self.at(x + cx, y + cy) == Some(0))
    }

    fn level(&self) -> u32 {
        self.rows / 10 + 1
    }

    fn tick_ms(&self) -> u32 {
        600_u32
            .saturating_sub((self.level() - 1).saturating_mul(50))
            .max(120)
    }

    fn shift(&mut self, by: i32) {
        if self.fits(self.turn, self.x + by, self.y) {
            self.x += by;
        }
    }

    fn spin(&mut self) {
        let turn = self.turn + 1;
        if let Some(by) = [0, -1, 1, -2, 2]
            .into_iter()
            .find(|by| self.fits(turn, self.x + by, self.y))
        {
            self.turn = turn;
            self.x += by;
        }
    }

    /// How far the piece can fall from where it is.
    fn fall(&self) -> i32 {
        let mut by = 0;
        while self.fits(self.turn, self.x, self.y + by + 1) {
            by += 1;
        }
        by
    }

    /// Moves the piece down a row, or sets it down when it cannot fall.
    fn step(&mut self) {
        if self.fits(self.turn, self.x, self.y + 1) {
            self.y += 1;
        } else {
            self.settle();
        }
    }

    fn settle(&mut self) {
        for (cx, cy) in cells(self.piece, self.turn) {
            let (x, y) = (self.x + cx, self.y + cy);
            if (0..COLS).contains(&x) && (0..ROWS).contains(&y) {
                self.board[(y * COLS + x) as usize] = self.piece as u8 + 1;
            }
        }
        let kept: Vec<&[u8]> = self
            .board
            .chunks(COLS as usize)
            .filter(|row| row.contains(&0))
            .collect();
        let closed = ROWS as usize - kept.len();
        let mut board = vec![0; closed * COLS as usize];
        for row in kept {
            board.extend_from_slice(row);
        }
        self.board = board;
        self.score = self
            .score
            .saturating_add(POINTS[closed.min(4)].saturating_mul(self.level()));
        self.rows = self.rows.saturating_add(closed as u32);
        self.piece = self.next;
        self.next = roll(&mut self.seed);
        (self.turn, self.x, self.y) = (0, 3, 0);
        self.over = !self.fits(0, 3, 0);
    }

    fn act(&mut self, event: &Event) {
        let key = match event {
            Event::Tick => {
                self.ticks = self.ticks.wrapping_add(1);
                if !self.over {
                    self.step();
                }
                return;
            }
            Event::Key(key) => *key,
            Event::Hit(id) => match id.as_str() {
                "left" => Key::Left,
                "right" => Key::Right,
                "turn" => Key::Up,
                "down" => Key::Down,
                "drop" => Key::Space,
                "new" => Key::Enter,
                _ => return,
            },
        };
        self.seed ^= self.ticks.wrapping_mul(2_654_435_761).rotate_left(7);
        if key == Key::Enter {
            *self = Game::new(self.seed ^ 0x9e37_79b9);
            return;
        }
        if self.over {
            return;
        }
        match key {
            Key::Left => self.shift(-1),
            Key::Right => self.shift(1),
            Key::Up => self.spin(),
            Key::Down => self.step(),
            Key::Space => {
                self.y += self.fall();
                self.settle();
            }
            Key::Enter => {}
        }
    }

    fn write(&self) -> String {
        let board: String = self
            .board
            .iter()
            .map(|cell| char::from(b'0' + (*cell).min(7)))
            .collect();
        format!(
            "{board}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            self.piece,
            self.turn,
            self.x,
            self.y,
            self.next,
            self.seed,
            self.score,
            self.rows,
            self.ticks,
            u8::from(self.over)
        )
    }

    fn read(state: &str) -> Option<Game> {
        let mut parts = state.split('|');
        let board: Vec<u8> = parts
            .next()?
            .bytes()
            .map(|byte| byte.wrapping_sub(b'0'))
            .collect();
        if board.len() != (COLS * ROWS) as usize || board.iter().any(|cell| *cell > 7) {
            return None;
        }
        let mut number = || parts.next()?.parse::<i64>().ok();
        let game = Game {
            board,
            piece: usize::try_from(number()?).ok()? % 7,
            turn: i32::try_from(number()?).ok()?.rem_euclid(4),
            x: i32::try_from(number()?).ok()?.clamp(-4, COLS),
            y: i32::try_from(number()?).ok()?.clamp(-4, ROWS),
            next: usize::try_from(number()?).ok()? % 7,
            seed: u32::try_from(number()?).ok()?,
            score: u32::try_from(number()?).ok()?,
            rows: u32::try_from(number()?).ok()?,
            ticks: u32::try_from(number()?).ok()?,
            over: number()? != 0,
        };
        Some(game)
    }
}

fn block(ctx: &mut Context2d, piece: usize, x: f32, y: f32, alpha: f32) {
    ctx.set_global_alpha(alpha);
    ctx.set_fill_style(INKS[piece % 7]);
    ctx.fill_rect(x + 1.0, y + 1.0, CELL - 2.0, CELL - 2.0);
    ctx.set_global_alpha(1.0);
}

fn button(ctx: &mut Context2d, id: &str, label: &str, x: f32, y: f32, width: f32) {
    ctx.save();
    ctx.translate(x, y);
    ctx.set_fill_style(Tone::Muted);
    ctx.set_global_alpha(0.22);
    ctx.fill_rect(0.0, 0.0, width, 30.0);
    ctx.set_global_alpha(1.0);
    ctx.set_fill_style(Tone::Neutral);
    ctx.set_text_align(TextAlign::Center);
    ctx.set_font("bold 12px sans-serif");
    ctx.fill_text(label, width / 2.0, 20.0);
    ctx.hit(0.0, 0.0, width, 30.0, id);
    ctx.restore();
}

fn draw(game: &Game) -> Node {
    let mut ctx = Context2d::new(BOARD + SIDE, HEIGHT);
    ctx.set_fill_style(Tone::Muted);
    ctx.set_global_alpha(0.10);
    ctx.fill_rect(0.0, 0.0, BOARD, HEIGHT);
    ctx.set_global_alpha(1.0);
    for y in 0..ROWS {
        for x in 0..COLS {
            if let Some(cell @ 1..) = game.at(x, y) {
                block(
                    &mut ctx,
                    usize::from(cell - 1),
                    x as f32 * CELL,
                    y as f32 * CELL,
                    1.0,
                );
            }
        }
    }
    if !game.over {
        let fall = game.fall();
        for (cx, cy) in cells(game.piece, game.turn) {
            let (x, y) = ((game.x + cx) as f32 * CELL, (game.y + cy) as f32 * CELL);
            if fall > 0 {
                block(&mut ctx, game.piece, x, y + fall as f32 * CELL, 0.22);
            }
            block(&mut ctx, game.piece, x, y, 1.0);
        }
    }

    let left = BOARD + 20.0;
    ctx.set_fill_style(Tone::Muted);
    ctx.set_text_align(TextAlign::Left);
    ctx.set_font("12px sans-serif");
    ctx.fill_text("Next", left, 16.0);
    for (cx, cy) in cells(game.next, 0) {
        block(
            &mut ctx,
            game.next,
            left + cx as f32 * CELL,
            26.0 + cy as f32 * CELL,
            1.0,
        );
    }
    let half = (SIDE - 30.0) / 2.0 - 4.0;
    let top = 150.0;
    button(&mut ctx, "left", "left", left, top, half);
    button(&mut ctx, "right", "right", left + half + 8.0, top, half);
    button(&mut ctx, "turn", "turn", left, top + 38.0, half);
    button(
        &mut ctx,
        "down",
        "down",
        left + half + 8.0,
        top + 38.0,
        half,
    );
    button(&mut ctx, "drop", "drop", left, top + 76.0, half * 2.0 + 8.0);
    button(
        &mut ctx,
        "new",
        "new game",
        left,
        top + 130.0,
        half * 2.0 + 8.0,
    );
    if game.over {
        ctx.set_fill_style(Style::Rgba([0, 0, 0, 0xb0]));
        ctx.fill_rect(0.0, HEIGHT / 2.0 - 34.0, BOARD, 68.0);
        ctx.set_fill_style(Tone::Bad);
        ctx.set_text_align(TextAlign::Center);
        ctx.set_font("bold 20px sans-serif");
        ctx.fill_text("Stacked out", BOARD / 2.0, HEIGHT / 2.0 + 7.0);
    }
    ctx.finish()
}

fn stat(label: &str, value: u32, tone: Tone) -> Stat {
    Stat {
        label: label.to_string(),
        value: value.to_string(),
        tone,
    }
}

fn stack(request: Request) -> Result<View, String> {
    let mut game = request
        .state
        .as_deref()
        .and_then(Game::read)
        .unwrap_or_else(|| Game::new(0x2545_f491));
    if let Some(event) = &request.event {
        game.act(event);
    }
    Ok(View {
        title: "Stack".to_string(),
        body: Node::Stack {
            children: vec![
                Node::Stats {
                    items: vec![
                        stat("Score", game.score, Tone::Info),
                        stat("Rows", game.rows, Tone::Good),
                        stat("Level", game.level(), Tone::Neutral),
                    ],
                },
                draw(&game),
                Node::Text {
                    text: "Arrows move and turn, space drops, enter starts over.".to_string(),
                    tone: Tone::Muted,
                },
            ],
        },
        controls: vec![],
        live: Live {
            state: Some(game.write()),
            tick_ms: (!game.over).then(|| game.tick_ms()),
            keys: vec![
                Key::Left,
                Key::Right,
                Key::Up,
                Key::Down,
                Key::Space,
                Key::Enter,
            ],
        },
    })
}

#[cfg(test)]
mod tests {
    use crucible_view::{Draw, Event, Key, Limits, Node, Request, View, ViewKind};

    use crate::{COLS, Game, ROWS, cells, stack};

    fn asked(state: Option<&str>, event: Option<Event>) -> View {
        let view = stack(Request {
            state: state.map(str::to_string),
            event,
            ..Request::default()
        })
        .unwrap();
        view.check(ViewKind::Run, &Limits::DEFAULT).unwrap();
        view
    }

    fn game(view: &View) -> Game {
        Game::read(view.live.state.as_deref().unwrap()).unwrap()
    }

    /// A game whose flat piece is at the top over a floor with a gap that piece fills.
    fn gapped() -> Game {
        let mut game = Game::new(7);
        game.piece = 0;
        for x in (0..COLS).filter(|x| !(3..7).contains(x)) {
            game.board[((ROWS - 1) * COLS + x) as usize] = 2;
        }
        game
    }

    #[test]
    fn a_game_reads_back_as_written_and_a_torn_state_starts_over() {
        let mut played = gapped();
        played.act(&Event::Key(Key::Up));
        played.act(&Event::Tick);
        played.score = u32::MAX;
        assert_eq!(Game::read(&played.write()), Some(played.clone()));
        let written = played.write();
        let (board, rest) = written.split_once('|').unwrap();
        assert_eq!(board.len(), (COLS * ROWS) as usize);
        for torn in [
            String::new(),
            "junk".to_string(),
            board.to_string(),
            format!("{}|{rest}", &board[1..]),
            format!("{}8|{rest}", &board[1..]),
            format!("{board}|{}", rest.replace('|', ",")),
            format!("{board}|{}", rest.rsplit_once('|').unwrap().0),
            format!("{board}|-1|0|3|0|1|7|0|0|0|0"),
            format!("{board}|0|0|3|0|1|4294967296|0|0|0|0"),
        ] {
            assert_eq!(Game::read(&torn), None, "{torn}");
        }
        let fresh = game(&asked(None, None));
        assert_eq!(game(&asked(Some("junk"), None)), fresh);
        assert_eq!(
            game(&asked(Some("junk"), Some(Event::Hit("x".into())))),
            fresh
        );
        assert!(fresh.board.iter().all(|cell| *cell == 0));
        let far = format!("{board}|9|7|99|-99|8|7|0|0|0|0");
        let read = Game::read(&far).unwrap();
        assert_eq!(
            (read.piece, read.turn, read.x, read.y, read.next),
            (2, 3, COLS, -4, 1),
            "a place off the board is brought back to its edge"
        );
        asked(Some(&far), Some(Event::Key(Key::Space)));
    }

    #[test]
    fn a_piece_moves_inside_the_walls_and_turns_in_its_box() {
        let mut game = gapped();
        for _ in 0..COLS {
            game.act(&Event::Key(Key::Left));
        }
        assert_eq!(game.x, 0);
        for _ in 0..COLS * 2 {
            game.act(&Event::Hit("right".into()));
        }
        assert_eq!(game.x, COLS - 4);
        assert_eq!(cells(0, 0), [(0, 1), (1, 1), (2, 1), (3, 1)]);
        assert_eq!(cells(0, 1), [(2, 0), (2, 1), (2, 2), (2, 3)]);
        assert_eq!(cells(0, 4), cells(0, 0), "four turns come round");
        assert_eq!(cells(0, -1), cells(0, 3));
        assert_eq!(cells(1, 3), cells(1, 0), "the square does not turn");
        assert_eq!(cells(8, 0), cells(1, 0));
        game.act(&Event::Key(Key::Up));
        assert_eq!((game.turn, game.x), (1, COLS - 4));
        for _ in 0..COLS {
            game.act(&Event::Key(Key::Right));
        }
        assert_eq!(game.x, COLS - 3, "upright, it reaches the wall");
        game.act(&Event::Hit("turn".into()));
        assert_eq!(
            (game.turn, game.x),
            (2, COLS - 4),
            "a turn into the wall moves it off the wall"
        );
        let before = game.clone();
        game.act(&Event::Hit("nothing".into()));
        game.act(&Event::Hit(String::new()));
        assert_eq!(game, before, "a hit with no button does nothing");
    }

    #[test]
    fn a_full_row_closes_and_scores_by_the_level() {
        let mut game = gapped();
        game.act(&Event::Key(Key::Down));
        assert_eq!(game.y, 1);
        game.act(&Event::Tick);
        assert_eq!((game.y, game.ticks), (2, 1));
        assert_eq!(game.fall(), ROWS - 4);
        game.act(&Event::Hit("drop".into()));
        assert_eq!((game.rows, game.score), (1, 100));
        assert!(game.board.iter().all(|cell| *cell == 0), "the row is gone");
        assert_eq!(
            (game.x, game.y, game.turn),
            (3, 0, 0),
            "the next piece is up"
        );

        let mut later = gapped();
        later.rows = 29;
        assert_eq!(later.level(), 3);
        later.act(&Event::Key(Key::Space));
        assert_eq!((later.rows, later.score, later.level()), (30, 300, 4));

        let mut open = gapped();
        open.board[((ROWS - 1) * COLS) as usize] = 0;
        open.act(&Event::Key(Key::Space));
        assert_eq!((open.rows, open.score), (0, 0), "a row with a gap stays");
        let filled = open.board.iter().filter(|cell| **cell != 0).count();
        assert_eq!(filled, COLS as usize - 1);

        let mut most = gapped();
        (most.rows, most.score) = (u32::MAX, u32::MAX);
        most.act(&Event::Key(Key::Space));
        assert_eq!((most.rows, most.score), (u32::MAX, u32::MAX));
        assert_eq!(most.tick_ms(), 120);
    }

    #[test]
    fn the_clock_quickens_with_the_level_and_stops_on_a_full_board() {
        let at = |rows: u32| {
            let mut game = Game::new(7);
            game.rows = rows;
            game.tick_ms()
        };
        assert_eq!(
            [at(0), at(9), at(10), at(90), at(99), at(100), at(1_000)],
            [600, 600, 550, 150, 150, 120, 120]
        );
        let first = asked(None, None);
        assert_eq!(first.live.tick_ms, Some(600));
        assert_eq!(first.live.keys.len(), 6);
        let mut state = first.live.state.clone().unwrap();
        let mut drops = 0;
        let over = loop {
            let view = asked(Some(&state), Some(Event::Key(Key::Space)));
            state = view.live.state.clone().unwrap();
            drops += 1;
            assert!(drops < 100, "the board fills");
            if view.live.tick_ms.is_none() {
                break view;
            }
        };
        assert!(game(&over).over);
        assert_eq!(over.live.keys.len(), 6, "enter still starts over");
        for event in [
            Event::Tick,
            Event::Key(Key::Left),
            Event::Hit("drop".into()),
        ] {
            let still = game(&asked(Some(&state), Some(event)));
            assert_eq!(still.board, game(&over).board);
            assert!(still.over);
        }
        let again = asked(Some(&state), Some(Event::Hit("new".into())));
        assert_eq!(again.live.tick_ms, Some(600));
        let fresh = game(&again);
        assert!(!fresh.over && fresh.board.iter().all(|cell| *cell == 0));
        assert_eq!((fresh.score, fresh.rows), (0, 0));
    }

    #[test]
    fn the_board_carries_a_hit_for_each_move() {
        let view = asked(None, None);
        let Node::Stack { children } = &view.body else {
            panic!("the view is a stack");
        };
        let Node::Canvas { commands, .. } = &children[1] else {
            panic!("the board is a canvas");
        };
        let hits: Vec<&str> = commands
            .iter()
            .filter_map(|command| match command {
                Draw::Hit { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(hits, ["left", "right", "turn", "down", "drop", "new"]);
    }
}
