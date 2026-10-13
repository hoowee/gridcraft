//! Custom commands (this fork): the 四算表 (Sisuan) cost-estimate worksheet tools, ported from
//! VibeOffice Ledger's `sisuan.ts` (itself from the Univer-based editor in cip-design-daas-2).
//!
//! A Sisuan sheet is a cost-estimate tree: column A holds a multi-level numbering (Chinese
//! numerals 一…十, then （n） paragraphs, then dotted decimal leaves), row 4 holds headers named
//! after the estimate columns （1）…（16）, and each row's estimate cells are formulas over its
//! subordinate rows or its own inputs. These commands maintain that tree: repair the numbering,
//! grow or shrink levels, fill the default formulas, and fold or unfold the levels.
//!
//! Each command is one `edit`, so one undo step, like the originals (which ran in a single tx).

use gridcraft_model::{Color, NumFmt};
use serde_json::{Value as Json, json};

use super::*;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "sisuan.autoFix",
            "AutoFix Numbers",
            ["Sisuan", "Numbering"],
            None,
            "{range?: \"A1:A20\" (default selection; its first column is checked)}",
            has_doc,
            auto_fix
        ),
        cmd!(
            "sisuan.increaseLevel",
            "Increase Number Level",
            ["Sisuan", "Numbering"],
            None,
            "{range?: \"A1:A20\" (default selection)}",
            has_doc,
            |s, p| change_level(s, p, 1)
        ),
        cmd!(
            "sisuan.decreaseLevel",
            "Decrease Number Level",
            ["Sisuan", "Numbering"],
            None,
            "{range?: \"A1:A20\" (default selection)}",
            has_doc,
            |s, p| change_level(s, p, -1)
        ),
        cmd!(
            "sisuan.writeFormulas",
            "Write Default Formulas",
            ["Sisuan", "Formulas"],
            None,
            "{range?: \"A1:K20\" (default selection; headers are read from row 4)}",
            has_doc,
            write_formulas
        ),
        cmd!(
            "sisuan.toggleNextLevel",
            "Toggle Next Level",
            ["Sisuan", "Levels"],
            None,
            "{range?: \"A5\" (default selection; its first row drives the fold)}",
            has_doc,
            |s, p| toggle_levels(s, p, false)
        ),
        cmd!(
            "sisuan.toggleAllSubordinates",
            "Toggle All Subordinates",
            ["Sisuan", "Levels"],
            None,
            "{range?: \"A5\" (default selection; its first row drives the fold)}",
            has_doc,
            |s, p| toggle_levels(s, p, true)
        ),
    ]
}

// ---------------------------------------------------------------- constants

/// The estimate headers live on this row (0-based), as the originals laid the sheet out.
const HEADER_ROW: u32 = 3;
/// The originals scan the header row's first 45 columns.
const HEADER_MAX_COL: u32 = 44;
/// A rewritten number shows in magenta; one that was already magenta turns purple. Auto is the
/// normal colour.
const MAGENTA: u32 = 0xFF00FF;
const PURPLE: u32 = 0x800080;
/// Captions whose columns keep their own colour (the originals never tint them).
const UNMARKED_HEADERS: [&str; 2] = ["（3）", "（9）"];
const CHINESE_NUMBERS: [&str; 10] = ["一", "二", "三", "四", "五", "六", "七", "八", "九", "十"];
/// A row followed by this text stays a leaf.
const TOTALS_LABEL: &str = "费用合计";
/// Per-command cap on selected cells (a whole-column `A:A` on a hostile sheet is not a job).
const MAX_RANGE_CELLS: u64 = 2_000_000;
/// Cap on the rows one fold command may touch (hostile numbering could otherwise fan out).
const MAX_FOLD_ROWS: usize = 65_536;

// ---------------------------------------------------------------- cell access

/// The text the user sees in the cell, trimmed; empty cells give "".
fn display_text(cx: &Ctx, sheet: usize, at: CellRef) -> String {
    let Some(sh) = cx.wb.sheet(sheet) else { return String::new() };
    crate::display::value_text(&cx.wb, sh, at).trim().to_string()
}

/// The cell's explicit font colour, `None` for the automatic / theme colour (like the originals,
/// which only recognised explicit RGB).
fn font_rgb(cx: &Ctx, sheet: usize, at: CellRef) -> Option<u32> {
    let sh = cx.wb.sheet(sheet)?;
    match cx.wb.styles.get(sh.style_id(at)).font.color {
        Color::Rgb(v) => Some(v),
        _ => None,
    }
}

fn set_font_color(cx: &mut Ctx, sheet: usize, at: CellRef, rgb: Option<u32>) {
    let cur = cx.wb.sheet(sheet).map(|sh| sh.style_id(at)).unwrap_or_default();
    let new = cx.wb.styles.derive(cur, |st| st.font.color = rgb.map_or(Color::Auto, Color::Rgb));
    if let Ok(sh) = cx.sheet_mut(sheet) {
        sh.set_style(at, new);
    }
}

fn set_num_fmt(cx: &mut Ctx, sheet: usize, at: CellRef, code: &str) {
    let cur = cx.wb.sheet(sheet).map(|sh| sh.style_id(at)).unwrap_or_default();
    let new = cx.wb.styles.derive(cur, |st| st.num_fmt = NumFmt::new(code));
    if let Ok(sh) = cx.sheet_mut(sheet) {
        sh.set_style(at, new);
    }
}

/// Writes input text the way typing does: `'1.10` lands as text (so the trailing 0 survives),
/// `=…` as a formula, a plain integer as a number (right-aligned like the originals).
fn put_input(cx: &mut Ctx, sheet: usize, at: CellRef, input: &str) -> Result<()> {
    if let Some(cell) = edit::input_to_cell(input, sheet, at, &mut cx.wb)? {
        cx.sheet_mut(sheet)?.cells.set(at, cell);
    } else {
        cx.sheet_mut(sheet)?.cells.remove(at);
    }
    cx.touch(sheet, at);
    Ok(())
}

/// Writes a numbering value: dotted numbers as text, plain integers as numbers.
fn put_numbering(cx: &mut Ctx, sheet: usize, at: CellRef, value: &str, color: u32) -> Result<()> {
    let input = if value.contains('.') { format!("'{value}") } else { value.to_string() };
    put_input(cx, sheet, at, &input)?;
    set_font_color(cx, sheet, at, Some(color));
    Ok(())
}

fn last_used_row(cx: &Ctx, sheet: usize) -> u32 {
    cx.wb.sheet(sheet).and_then(|sh| sh.used_range()).map_or(0, |r| r.end.row)
}

fn is_row_hidden(cx: &Ctx, sheet: usize, row: u32) -> bool {
    cx.wb.sheet(sheet).is_some_and(|sh| sh.is_row_hidden(row))
}

/// Per-cell colour reset over a selection, the way the originals start each command (they painted
/// the whole selection black first, then re-marked what they rewrote).
fn reset_range_font_color(cx: &mut Ctx, sheet: usize, range: &RangeRef) {
    for at in range.iter() {
        set_font_color(cx, sheet, at, None);
    }
}

// ---------------------------------------------------------------- numbering rules (as the originals)

/// `1`, `1.2`, `1.2.3` — the leaf numbering.
fn is_multi_level_number(s: &str) -> bool {
    let mut seg = 0usize;
    let mut any = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            seg += 1;
            any = true;
        } else if ch == '.' {
            if seg == 0 {
                return false;
            }
            seg = 0;
        } else {
            return false;
        }
    }
    any && seg > 0
}

/// 0 = not a numbering, 1 = Chinese numeral, 2 = （n） paragraph, 3 = dotted / plain number.
fn number_kind(s: &str) -> u8 {
    if s.is_empty() {
        return 0;
    }
    if CHINESE_NUMBERS.contains(&s) {
        return 1;
    }
    if s.starts_with('（') && s.ends_with('）') {
        return 2;
    }
    if s.contains('.') || s.bytes().all(|b| b.is_ascii_digit()) {
        return 3;
    }
    0
}

fn increment_last_digit(s: &str) -> String {
    let (head, last) = match s.rsplit_once('.') {
        Some((h, l)) => (format!("{h}."), l),
        None => (String::new(), s),
    };
    let n = last.parse::<u64>().unwrap_or(0).saturating_add(1);
    format!("{head}{n}")
}

/// Drops the last `level` levels; asking for more levels than there are changes nothing.
fn prefix_without(s: &str, level: usize) -> String {
    let parts: Vec<&str> = s.split('.').collect();
    if level >= parts.len() {
        return s.to_string();
    }
    parts[..parts.len() - level].join(".")
}

/// Whether `next` follows as a leaf after `cur`: an end of data, the totals row, or a jump that
/// leaves the numbering's kind.
fn is_leaf(cur: &str, next: &str) -> bool {
    let (up, down) = (number_kind(cur), number_kind(next));
    if next.is_empty() || next == TOTALS_LABEL {
        return true;
    }
    if up != down {
        return false;
    }
    if up == 3 && down == 3 {
        return cur.split('.').count() >= next.split('.').count();
    }
    false
}

fn next_non_empty_row(cx: &Ctx, sheet: usize, col: u32, after: u32, last_row: u32) -> Option<u32> {
    let from = after.checked_add(1)?;
    (from..=last_row).find(|&r| !display_text(cx, sheet, CellRef::new(r, col)).is_empty())
}

/// The direct subordinate rows of `start` under column `col`'s numbering.
fn sub_level_rows(cx: &Ctx, sheet: usize, start: u32, col: u32, last_row: u32) -> Vec<u32> {
    let start_value = display_text(cx, sheet, CellRef::new(start, col));
    let start_kind = number_kind(&start_value);
    let start_levels = start_value.split('.').count();
    let first_ch = start_value.chars().next();
    let mut subs = Vec::new();
    let mut cur = start.saturating_add(1);
    while cur <= last_row {
        let v = display_text(cx, sheet, CellRef::new(cur, col));
        if v == "..." {
            cur += 1;
            continue;
        }
        let kind = number_kind(&v);
        match (start_kind, kind) {
            (1, 2) => subs.push(cur),
            (1, 1) => break,
            (2, 2) => break,
            (2, 3) if !v.contains('.') => subs.push(cur),
            (3, 3) => {
                if v.chars().next() != first_ch {
                    break;
                }
                match (v.split('.').count()).checked_sub(start_levels) {
                    Some(1) => subs.push(cur),
                    Some(0) => break,
                    _ => {}
                }
            }
            _ => {}
        }
        cur += 1;
    }
    subs
}

/// `subs` and every deeper level below them, iteratively (hostile numbering could nest deeply).
fn all_subordinate_rows(cx: &Ctx, sheet: usize, col: u32, last_row: u32, subs: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack: Vec<u32> = subs.to_vec();
    while let Some(r) = stack.pop() {
        if !seen.insert(r) || out.len() >= MAX_FOLD_ROWS {
            continue;
        }
        out.push(r);
        stack.extend(sub_level_rows(cx, sheet, r, col, last_row));
    }
    out.sort_unstable();
    out
}

// ---------------------------------------------------------------- command 1: AutoFix Numbers

/// The number `next` should carry, given how much deeper (positive), shallower (negative) or level
/// (zero) it sits than `cur`. `Err` is the message for the row that must be fixed by hand.
fn expected_number(cur: &str, next: &str, row_1based: u64) -> std::result::Result<String, String> {
    if !is_multi_level_number(cur) || !is_multi_level_number(next) {
        return Err(format!("The value at row {row_1based} is wrong and must be fixed manually."));
    }
    let diff = next.split('.').count() as i64 - cur.split('.').count() as i64;
    if diff == 0 {
        Ok(increment_last_digit(cur))
    } else if diff == 1 {
        Ok(format!("{cur}.1"))
    } else if diff < 0 {
        Ok(increment_last_digit(&prefix_without(cur, (-diff) as usize)))
    } else {
        Err(format!("Row {row_1based} has a wrong number (a parent level may be missing); it cannot be fixed automatically."))
    }
}

fn auto_fix(s: &mut Session, p: &Json) -> Result<Json> {
    let sheet = target_sheet(s, p)?;
    let range = target_range(s, p)?;
    if range.start.row == range.end.row && range.start.col == range.end.col {
        return Err(bad("sisuan.autoFix", "Select more than a single cell."));
    }
    if range.count() > MAX_RANGE_CELLS {
        return Err(bad("sisuan.autoFix", "selection too large"));
    }
    let col = range.start.col;
    // Stop reason carried out of the edit, as the originals reported it: "done" after a full pass
    // with fixes, "checked" when the data simply ran out, "error" with the offending row.
    let mut status = "done";
    let mut message = String::new();
    let mut rewritten = 0u64;
    edit(s, |cx| {
        reset_range_font_color(cx, sheet, &range);
        let last_row = last_used_row(cx, sheet);
        let mut row = range.start.row;
        while row < range.end.row {
            let mut cur = row;
            if display_text(cx, sheet, CellRef::new(cur, col)).is_empty() {
                match next_non_empty_row(cx, sheet, col, cur, last_row) {
                    Some(jump) => cur = jump,
                    None => {
                        status = "checked";
                        break;
                    }
                }
            }
            let Some(next) = next_non_empty_row(cx, sheet, col, cur, last_row) else {
                status = "checked";
                break;
            };
            if next > cur + 1 {
                status = "error";
                message = format!(
                    "Rows {} and {} have a blank row between them; a number may be missing. Blank rows are not allowed in the estimate tree, so fill it in manually.",
                    cur + 1,
                    next + 1
                );
                break;
            }
            let cur_text = display_text(cx, sheet, CellRef::new(cur, col));
            let next_text = display_text(cx, sheet, CellRef::new(next, col));
            match expected_number(&cur_text, &next_text, (next + 1) as u64) {
                Ok(nv) if nv != next_text => {
                    put_numbering(cx, sheet, CellRef::new(next, col), &nv, MAGENTA)?;
                    rewritten += 1;
                }
                Ok(_) => {}
                Err(m) => {
                    status = "error";
                    message = m;
                    break;
                }
            }
            row += 1;
        }
        Ok(())
    })?;
    Ok(match status {
        "error" => json!({"status": "error", "message": message, "rewritten": rewritten}),
        _ => json!({"status": status, "rewritten": rewritten}),
    })
}

// ---------------------------------------------------------------- commands 2/3: number levels

fn change_level(s: &mut Session, p: &Json, delta: i32) -> Result<Json> {
    let sheet = target_sheet(s, p)?;
    let range = target_range(s, p)?;
    if range.count() > MAX_RANGE_CELLS {
        return Err(bad("sisuan.changeLevel", "selection too large"));
    }
    let col = range.start.col;
    let mut changed = 0u64;
    let mut invalid = 0u64;
    edit(s, |cx| {
        for row in range.start.row..=range.end.row {
            let at = CellRef::new(row, col);
            set_font_color(cx, sheet, at, None);
            let text = display_text(cx, sheet, at);
            if delta > 0 && text.is_empty() {
                break; // growing stops at the first blank row, as the originals do
            }
            if delta < 0 && text.is_empty() {
                continue;
            }
            if !is_multi_level_number(&text) {
                invalid += 1;
                continue;
            }
            let nv = if delta > 0 { format!("{text}.1") } else { prefix_without(&text, 1) };
            put_numbering(cx, sheet, at, &nv, MAGENTA)?;
            changed += 1;
        }
        Ok(())
    })?;
    let mut out = json!({"status": "ok", "changed": changed, "invalid": invalid});
    if invalid > 0 {
        out["message"] = json!(format!("{invalid} cell(s) do not follow the multi-level numbering rules; the rest were processed."));
    }
    Ok(out)
}

// ---------------------------------------------------------------- command 4: Write Default Formulas

fn write_formulas(s: &mut Session, p: &Json) -> Result<Json> {
    let sheet = target_sheet(s, p)?;
    let range = target_range(s, p)?;
    if range.count() > MAX_RANGE_CELLS {
        return Err(bad("sisuan.writeFormulas", "selection too large"));
    }
    let (start_col, end_col) = (range.start.col, range.end.col);
    let mut written = 0u64;
    edit(s, |cx| {
        let headers = header_map(cx, sheet);
        for row in range.start.row..=range.end.row {
            written += write_row_formulas(cx, sheet, row, start_col, end_col, &headers)?;
        }
        Ok(())
    })?;
    Ok(json!({"status": "ok", "written": written}))
}

/// Header caption → column, from row 4: （1）…（16） and the compound captions that carry their own
/// formula, like `（5）=10000*（6）/（3）`.
fn header_map(cx: &Ctx, sheet: usize) -> std::collections::HashMap<String, u32> {
    let mut out = std::collections::HashMap::new();
    for col in 0..=HEADER_MAX_COL {
        let h = display_text(cx, sheet, CellRef::new(HEADER_ROW, col));
        if !h.is_empty() {
            out.insert(h, col);
        }
    }
    out
}

/// Queues a formula job when its caption and every caption it references exist on the header row.
fn add_job(
    jobs: &mut Vec<(&'static str, String, Option<&'static str>)>,
    headers: &std::collections::HashMap<String, u32>,
    h: &'static str,
    needs: &[&str],
    f: String,
    nf: Option<&'static str>,
) {
    if headers.contains_key(h) && needs.iter().all(|n| headers.contains_key(*n)) {
        jobs.push((h, f, nf));
    }
}

fn write_row_formulas(
    cx: &mut Ctx,
    sheet: usize,
    row: u32,
    start_col: u32,
    end_col: u32,
    headers: &std::collections::HashMap<String, u32>,
) -> Result<u64> {
    let col_of = |h: &str| headers.get(h).copied();
    let own = |h: &str| col_of(h).map_or_else(String::new, |c| CellRef::new(row, c).a1());
    let cur = display_text(cx, sheet, CellRef::new(row, 0));
    let next = display_text(cx, sheet, CellRef::new(row.saturating_add(1), 0));
    let leaf = is_leaf(&cur, &next);

    // Colours before writing: a cell the originals had marked magenta turns purple this time.
    let prev: Vec<u32> =
        headers.iter().filter(|(h, c)| !UNMARKED_HEADERS.contains(&h.as_str()) && **c >= start_col && **c <= end_col).map(|(_, c)| *c).collect();

    // (caption, formula text, number format)
    let mut jobs: Vec<(&'static str, String, Option<&'static str>)> = Vec::new();
    add_job(&mut jobs, headers, "（4）", &["（2）"], format!("={}", own("（2）")), None);
    add_job(&mut jobs, headers, "（7）=（4）-（6）", &["（4）", "（6）"], format!("={}-{}", own("（4）"), own("（6）")), None);
    add_job(&mut jobs, headers, "（11）=10000*（12）/（9）", &["（12）", "（9）"], format!("={}/{}*10000", own("（12）"), own("（9）")), None);
    add_job(&mut jobs, headers, "（13）=（10）-（12）", &["（10）", "（12）"], format!("={}-{}", own("（10）"), own("（12）")), None);
    add_job(
        &mut jobs,
        headers,
        "（14）=（13）/（10）",
        &["（13）=（10）-（12）", "（10）"],
        format!("={}/{}", own("（13）=（10）-（12）"), own("（10）")),
        Some("0.00%"),
    );
    add_job(
        &mut jobs,
        headers,
        "（16）=（12）*形象进度百分比-（15）",
        &["（12）", "（15）"],
        format!("={}*50%-{}", own("（12）"), own("（15）")),
        None,
    );
    add_job(
        &mut jobs,
        headers,
        "（8）=（7）/（4）",
        &["（7）=（4）-（6）", "（4）"],
        format!("={}/{}", own("（7）=（4）-（6）"), own("（4）")),
        Some("0.00%"),
    );

    if !leaf {
        let last_row = last_used_row(cx, sheet);
        let subs = sub_level_rows(cx, sheet, row, 0, last_row);
        if !subs.is_empty() {
            // Parent rows sum their direct subordinates in the input columns.
            for h in ["（1）", "（2）", "（6）", "（10）", "（12）", "（15）"] {
                if let Some(col) = col_of(h) {
                    let f = if subs.len() == 1 {
                        format!("={}", CellRef::new(subs[0], col).a1())
                    } else {
                        let refs: Vec<String> = subs.iter().map(|&r| CellRef::new(r, col).a1()).collect();
                        format!("=SUM({})", refs.join(","))
                    };
                    jobs.push((h, f, None));
                }
            }
            add_job(&mut jobs, headers, "（5）=10000*（6）/（3）", &["（6）", "（3）"], format!("={}/{}*10000", own("（6）"), own("（3）")), None);
        }
    } else {
        add_job(
            &mut jobs,
            headers,
            "（6）",
            &["（3）", "（5）=10000*（6）/（3）"],
            format!("=({}*{})/10000", own("（3）"), own("（5）=10000*（6）/（3）")),
            None,
        );
    }

    let mut written = 0u64;
    for (h, f, nf) in jobs {
        let Some(col) = col_of(h) else { continue };
        if col < start_col || col > end_col {
            continue;
        }
        put_input(cx, sheet, CellRef::new(row, col), &f)?;
        if let Some(code) = nf {
            set_num_fmt(cx, sheet, CellRef::new(row, col), code);
        }
        written += 1;
    }

    // Marking: this row's estimate cells show magenta; what was already magenta turns purple.
    let was_magenta: Vec<u32> = prev.iter().copied().filter(|&c| font_rgb(cx, sheet, CellRef::new(row, c)) == Some(MAGENTA)).collect();
    for col in prev {
        set_font_color(cx, sheet, CellRef::new(row, col), Some(if was_magenta.contains(&col) { PURPLE } else { MAGENTA }));
    }
    Ok(written)
}

// ---------------------------------------------------------------- commands 5/6: fold levels

/// Shows or hides whole rows in one go (SUBTOTAL and the fold commands depend on it).
fn set_rows_hidden(cx: &mut Ctx, sheet: usize, rows: &[u32], hidden: bool) {
    let Ok(sh) = cx.sheet_mut(sheet) else { return };
    for &r in rows {
        if hidden {
            sh.rows.entry(r).or_default().hidden = true;
        } else if let Some(e) = sh.rows.get_mut(&r) {
            e.hidden = false;
            if e.size == Some(0.0) {
                e.size = None;
            }
        }
    }
    cx.structural = true;
}

fn toggle_levels(s: &mut Session, p: &Json, all: bool) -> Result<Json> {
    let sheet = target_sheet(s, p)?;
    let range = target_range(s, p)?;
    let row = range.start.row;
    let mut status = "leaf";
    let mut touched = 0usize;
    edit(s, |cx| {
        let last_row = last_used_row(cx, sheet);
        let cur = display_text(cx, sheet, CellRef::new(row, 0));
        let next = display_text(cx, sheet, CellRef::new(row.saturating_add(1), 0));
        if is_leaf(&cur, &next) {
            return Ok(());
        }
        let subs = sub_level_rows(cx, sheet, row, 0, last_row);
        if subs.is_empty() {
            status = "none";
            return Ok(());
        }
        if !is_row_hidden(cx, sheet, subs[0]) {
            // Visible now → fold everything below this row's level.
            let everything = all_subordinate_rows(cx, sheet, 0, last_row, &subs);
            set_rows_hidden(cx, sheet, &everything, true);
            touched = everything.len();
            status = "hidden";
        } else if all {
            // Hidden → unfold every subordinate level (command 6).
            let everything = all_subordinate_rows(cx, sheet, 0, last_row, &subs);
            set_rows_hidden(cx, sheet, &everything, false);
            touched = everything.len();
            status = "shown";
        } else {
            // Command 5: reveal only the next level, keep the deeper ones folded.
            let mut deeper = Vec::new();
            for &r in &subs {
                let kids = sub_level_rows(cx, sheet, r, 0, last_row);
                deeper.extend(all_subordinate_rows(cx, sheet, 0, last_row, &kids));
            }
            deeper.sort_unstable();
            deeper.dedup();
            set_rows_hidden(cx, sheet, &subs, false);
            if !deeper.is_empty() {
                set_rows_hidden(cx, sheet, &deeper, true);
            }
            touched = subs.len() + deeper.len();
            status = "shown";
        }
        Ok(())
    })?;
    Ok(json!({"status": status, "rows": touched}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s() -> crate::Session {
        let mut s = crate::Session::new();
        s.new_workbook();
        s
    }

    /// A numbering column: parent `1` over children `1.1` and `1.2`, headers （1）（2）（3）（4） on row 4.
    fn estimate_tree(s: &mut crate::Session) {
        for (cell, input) in [("A1", "'1"), ("A2", "'1.1"), ("A3", "'1.2"), ("B4", "'（1）"), ("C4", "'（2）"), ("D4", "'（3）"), ("E4", "'（4）")]
        {
            s.execute("cell.set", json!({"cell": cell, "input": input})).unwrap();
        }
    }

    fn get(s: &mut crate::Session, cell: &str) -> Json {
        s.execute("cell.get", json!({"cell": cell})).unwrap()
    }

    fn font_color(s: &crate::Session, row: u32, col: u32) -> Json {
        let d = s.doc().unwrap();
        let sh = d.wb.sheet(0).unwrap();
        match d.wb.styles.get(sh.style_id(CellRef::new(row, col))).font.color {
            Color::Rgb(v) => json!(format!("{v:06x}")),
            Color::Auto => json!("auto"),
            Color::Theme(i, t) => json!(format!("theme{i}/{t}")),
        }
    }

    #[test]
    fn increase_and_decrease_number_level() {
        let mut s = s();
        estimate_tree(&mut s);
        s.execute("sisuan.increaseLevel", json!({"range": "A1:A3"})).unwrap();
        assert_eq!(get(&mut s, "A1")["value"], json!("1.1"));
        assert_eq!(get(&mut s, "A3")["value"], json!("1.2.1"));
        assert_eq!(font_color(&s, 0, 0), json!("ff00ff"));
        s.execute("sisuan.decreaseLevel", json!({"range": "A1:A3"})).unwrap();
        // A bare integer lands as a number (right-aligned, like the originals); levels stay text.
        assert_eq!(get(&mut s, "A1")["value"], json!(1.0));
        assert_eq!(get(&mut s, "A3")["value"], json!("1.2"));
        // Increasing stops at the first blank row.
        s.execute("cell.set", json!({"cell": "A2", "input": ""})).unwrap();
        s.execute("sisuan.increaseLevel", json!({"range": "A1:A3"})).unwrap();
        assert_eq!(get(&mut s, "A3")["value"], json!("1.2"));
    }

    #[test]
    fn auto_fix_rewrites_the_wrong_numbers_in_magenta() {
        let mut s = s();
        s.execute("cell.set", json!({"cell": "A1", "input": "'1"})).unwrap();
        s.execute("cell.set", json!({"cell": "A2", "input": "'1.1"})).unwrap();
        s.execute("cell.set", json!({"cell": "A3", "input": "'1.1"})).unwrap();
        let r = s.execute("sisuan.autoFix", json!({"range": "A1:A3"})).unwrap();
        assert_eq!(r["status"], "done");
        assert_eq!(r["rewritten"], json!(1));
        assert_eq!(get(&mut s, "A3")["value"], json!("1.2"));
        assert_eq!(font_color(&s, 2, 0), json!("ff00ff"));
        assert_eq!(font_color(&s, 0, 0), json!("auto"));
        // A second pass finds nothing to fix.
        let r = s.execute("sisuan.autoFix", json!({"range": "A1:A3"})).unwrap();
        assert_eq!(r["rewritten"], json!(0));
    }

    #[test]
    fn auto_fix_reports_what_it_cannot_fix() {
        // A blank row inside the tree is refused.
        {
            let mut s = s();
            s.execute("cell.set", json!({"cell": "A1", "input": "'1"})).unwrap();
            s.execute("cell.set", json!({"cell": "A3", "input": "'1.1"})).unwrap();
            let r = s.execute("sisuan.autoFix", json!({"range": "A1:A3"})).unwrap();
            assert_eq!(r["status"], "error");
            assert!(r["message"].as_str().unwrap().contains("blank row"));
        }
        // A value that is not a numbering is refused, after fixing what came before it.
        {
            let mut s = s();
            s.execute("cell.set", json!({"cell": "A1", "input": "'1"})).unwrap();
            s.execute("cell.set", json!({"cell": "A2", "input": "'总说明"})).unwrap();
            let r = s.execute("sisuan.autoFix", json!({"range": "A1:A2"})).unwrap();
            assert_eq!(r["status"], "error");
            assert!(r["message"].as_str().unwrap().contains("manually"));
        }
        // Data that simply ends checks out.
        {
            let mut s = s();
            s.execute("cell.set", json!({"cell": "A1", "input": "'1"})).unwrap();
            let r = s.execute("sisuan.autoFix", json!({"range": "A1:A2"})).unwrap();
            assert_eq!(r["status"], "checked");
        }
    }

    #[test]
    fn write_default_formulas_sums_parents_and_fills_leaves() {
        {
            let mut s = s();
            estimate_tree(&mut s);
            let r = s.execute("sisuan.writeFormulas", json!({"range": "A1:E3"})).unwrap();
            assert_eq!(r["written"], json!(5));
            assert_eq!(get(&mut s, "B1")["formula"], "=SUM(B2,B3)");
            assert_eq!(get(&mut s, "C1")["formula"], "=SUM(C2,C3)");
            assert_eq!(get(&mut s, "E1")["formula"], "=C1");
            assert_eq!(get(&mut s, "E2")["formula"], "=C2");
            assert_eq!(get(&mut s, "E3")["formula"], "=C3");
            // Input columns （3） keep their colour; the estimate columns the command wrote show magenta.
            assert_eq!(font_color(&s, 0, 1), json!("ff00ff"));
            assert_eq!(font_color(&s, 0, 3), json!("auto"));
        }
        // Columns off the selection are skipped: A1:B3 only writes the （1） sums.
        {
            let mut s = s();
            estimate_tree(&mut s);
            let r = s.execute("sisuan.writeFormulas", json!({"range": "A1:B3"})).unwrap();
            assert_eq!(r["written"], json!(1));
            assert_eq!(get(&mut s, "B1")["formula"], "=SUM(B2,B3)");
        }
    }

    #[test]
    fn toggling_levels_folds_and_unfolds_the_tree() {
        let mut s = s();
        estimate_tree(&mut s);
        let hidden = |s: &crate::Session, row: u32| s.doc().unwrap().wb.sheet(0).unwrap().is_row_hidden(row);
        s.execute("sisuan.toggleNextLevel", json!({"range": "A1"})).unwrap();
        assert!(hidden(&s, 1) && hidden(&s, 2));
        s.execute("sisuan.toggleNextLevel", json!({"range": "A1"})).unwrap();
        assert!(!hidden(&s, 1) && !hidden(&s, 2));
        // A leaf row folds nothing.
        s.execute("sisuan.toggleNextLevel", json!({"range": "A3"})).unwrap();
        assert!(!hidden(&s, 1) && !hidden(&s, 2));
        s.execute("sisuan.toggleAllSubordinates", json!({"range": "A1"})).unwrap();
        assert!(hidden(&s, 1) && hidden(&s, 2));
        s.execute("sisuan.toggleAllSubordinates", json!({"range": "A1"})).unwrap();
        assert!(!hidden(&s, 1) && !hidden(&s, 2));
    }

    #[test]
    fn each_command_is_one_undo_step() {
        let mut s = s();
        estimate_tree(&mut s);
        s.execute("sisuan.writeFormulas", json!({"range": "A1:E3"})).unwrap();
        s.execute("edit.undo", json!({})).unwrap();
        assert!(get(&mut s, "B1")["formula"].is_null());
        assert!(get(&mut s, "E3")["formula"].is_null());
    }
}
