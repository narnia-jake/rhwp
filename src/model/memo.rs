//! 메모(주석) — HWP5 `%unk`/`%%me` 필드 + 구역 끝 MEMO_LIST 문단 리스트.
//!
//! 메모 하나는 두 조각이다.
//!
//! - **앵커**: 본문 문단 안의 필드 컨트롤. command 가
//!   `MEMO/65535/<번호>/<작성시각 FILETIME 하위>/<상위>/<작성자>/\;;` 꼴이다.
//!   HWP5 에서 읽으면 ctrl_id 가 `%unk` 라 `FieldType::Unknown`, HWPX 에서 읽으면
//!   `FieldType::Memo` 다. 둘 다 [`is_memo_field`] 로 가린다.
//! - **본문**: HWP5 는 마지막 구역 끝에 `MEMO_LIST(번호)` + `LIST_HEADER` + 문단들로,
//!   HWPX 는 `fieldBegin` 안 `subList` 로 둔다. 모델에서는 둘 다
//!   [`Field::memo_paragraphs`] 에 싣는다. HWP5 파서는 구역 끝 블록을 일단
//!   [`Section::memo_lists`] 에 담고, 문서를 다 읽은 뒤 [`attach_memo_lists`] 가
//!   번호로 짝을 지어 필드로 옮긴다. 짝이 없는 블록만 구역에 남는다.

use super::control::{Control, Field};
use super::document::Section;
use super::header_footer::MasterPage;
use super::paragraph::Paragraph;
use super::shape::{Caption, DrawingObjAttr, ShapeObject};

/// 구역 끝 MEMO_LIST 블록 하나 (번호, 본문 문단).
pub type MemoList = (u32, Vec<Paragraph>);

pub fn is_memo_field(field: &Field) -> bool {
    field.field_type == super::control::FieldType::Memo || field.command.starts_with("MEMO/")
}

/// 메모 번호. 필드 끝의 memo_index 가 있으면 그것, 없으면 command 셋째 칸.
pub fn memo_id(field: &Field) -> u32 {
    if field.memo_index != 0 {
        return field.memo_index;
    }
    command_part(&field.command, 2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// 작성자 (command 여섯째 칸).
pub fn memo_author(field: &Field) -> Option<&str> {
    command_part(&field.command, 5).filter(|s| !s.is_empty())
}

/// 작성 시각, 1601-01-01 부터 100ns 단위 (Windows FILETIME). 없으면 None.
pub fn memo_filetime(field: &Field) -> Option<u64> {
    let low: u64 = command_part(&field.command, 3)?.parse().ok()?;
    let high: u64 = command_part(&field.command, 4)?.parse().ok()?;
    Some((high << 32) | (low & 0xffff_ffff))
}

fn command_part(command: &str, index: usize) -> Option<&str> {
    command.strip_prefix("MEMO/")?;
    command.split('/').nth(index)
}

/// 한컴이 쓰는 메모 command. 작성자 안의 `/` 는 칸을 깨므로 지운다.
pub fn build_memo_command(id: u32, filetime: u64, author: &str) -> String {
    let author: String = author.chars().filter(|c| *c != '/' && *c != '\\').collect();
    format!(
        "MEMO/65535/{}/{}/{}/{}/\\;;",
        id,
        filetime & 0xffff_ffff,
        filetime >> 32,
        author
    )
}

/// 메모 본문 문단을 평문으로 (문단 사이는 줄바꿈).
pub fn memo_plain_text(paragraphs: &[Paragraph]) -> String {
    paragraphs
        .iter()
        .map(|p| p.text.replace('\r', ""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 문서 안의 모든 문단을 (표·글상자·머리말 등 중첩까지) 방문한다. 메모 본문 안은 들어가지 않는다.
/// 콜백은 (구역 번호, 문단) 을 받는다.
pub fn for_each_paragraph_mut(sections: &mut [Section], f: &mut dyn FnMut(usize, &mut Paragraph)) {
    for (si, section) in sections.iter_mut().enumerate() {
        let mut g = |p: &mut Paragraph| f(si, p);
        walk_paragraphs(&mut section.paragraphs, &mut g);
        walk_master_pages(&mut section.section_def.master_pages, &mut g);
    }
}

/// 문서 안의 모든 필드를 (중첩까지) 방문한다.
pub fn for_each_field_mut(sections: &mut [Section], f: &mut dyn FnMut(&mut Field)) {
    for_each_paragraph_mut(sections, &mut |_, para| {
        for control in &mut para.controls {
            if let Control::Field(field) = control {
                f(field);
            }
        }
    });
}

type ParaFn<'a> = dyn FnMut(&mut Paragraph) + 'a;

fn walk_paragraphs(paragraphs: &mut [Paragraph], f: &mut ParaFn) {
    for para in paragraphs {
        f(para);
        walk_controls(&mut para.controls, f);
    }
}

fn walk_master_pages(master_pages: &mut [MasterPage], f: &mut ParaFn) {
    for master_page in master_pages {
        walk_paragraphs(&mut master_page.paragraphs, f);
    }
}

fn walk_caption(caption: &mut Caption, f: &mut ParaFn) {
    walk_paragraphs(&mut caption.paragraphs, f);
}

fn walk_drawing(drawing: &mut DrawingObjAttr, f: &mut ParaFn) {
    if let Some(text_box) = &mut drawing.text_box {
        walk_paragraphs(&mut text_box.paragraphs, f);
    }
    if let Some(caption) = &mut drawing.caption {
        walk_caption(caption, f);
    }
}

fn walk_shape(shape: &mut ShapeObject, f: &mut ParaFn) {
    match shape {
        ShapeObject::Picture(pic) => {
            if let Some(caption) = &mut pic.caption {
                walk_caption(caption, f);
            }
        }
        ShapeObject::Group(group) => {
            for child in &mut group.children {
                walk_shape(child, f);
            }
            if let Some(caption) = &mut group.caption {
                walk_caption(caption, f);
            }
        }
        _ => {
            if let Some(drawing) = shape.drawing_mut() {
                walk_drawing(drawing, f);
            }
        }
    }
}

fn walk_controls(controls: &mut [Control], f: &mut ParaFn) {
    for control in controls {
        match control {
            Control::Picture(pic) => {
                if let Some(caption) = &mut pic.caption {
                    walk_caption(caption, f);
                }
            }
            Control::Shape(shape) => walk_shape(shape, f),
            Control::Table(table) => {
                for cell in &mut table.cells {
                    walk_paragraphs(&mut cell.paragraphs, f);
                }
                if let Some(caption) = &mut table.caption {
                    walk_caption(caption, f);
                }
            }
            Control::Header(header) => walk_paragraphs(&mut header.paragraphs, f),
            Control::Footer(footer) => walk_paragraphs(&mut footer.paragraphs, f),
            Control::Footnote(footnote) => walk_paragraphs(&mut footnote.paragraphs, f),
            Control::Endnote(endnote) => walk_paragraphs(&mut endnote.paragraphs, f),
            Control::HiddenComment(comment) => walk_paragraphs(&mut comment.paragraphs, f),
            Control::SectionDef(section_def) => walk_master_pages(&mut section_def.master_pages, f),
            _ => {}
        }
    }
}

/// 읽기 전용 방문 — 메모 필드만, `for_each_field_mut` 과 같은 순서.
pub fn collect_memo_fields(sections: &[Section]) -> Vec<&Field> {
    let mut out = Vec::new();
    for section in sections {
        ro::paragraphs(&section.paragraphs, &mut out);
        ro::master_pages(&section.section_def.master_pages, &mut out);
    }
    out
}

/// 문단 목록 하나(구역 하나)의 메모 필드.
pub fn collect_memo_fields_in(paragraphs: &[Paragraph]) -> Vec<&Field> {
    let mut out = Vec::new();
    ro::paragraphs(paragraphs, &mut out);
    out
}

mod ro {
    use super::*;

    pub fn paragraphs<'a>(paragraphs: &'a [Paragraph], out: &mut Vec<&'a Field>) {
        for para in paragraphs {
            controls(&para.controls, out);
        }
    }

    pub fn master_pages<'a>(master_pages: &'a [MasterPage], out: &mut Vec<&'a Field>) {
        for master_page in master_pages {
            paragraphs(&master_page.paragraphs, out);
        }
    }

    fn caption<'a>(caption: &'a Caption, out: &mut Vec<&'a Field>) {
        paragraphs(&caption.paragraphs, out);
    }

    fn drawing<'a>(drawing: &'a DrawingObjAttr, out: &mut Vec<&'a Field>) {
        if let Some(text_box) = &drawing.text_box {
            paragraphs(&text_box.paragraphs, out);
        }
        if let Some(c) = &drawing.caption {
            caption(c, out);
        }
    }

    fn shape<'a>(shape_obj: &'a ShapeObject, out: &mut Vec<&'a Field>) {
        match shape_obj {
            ShapeObject::Picture(pic) => {
                if let Some(c) = &pic.caption {
                    caption(c, out);
                }
            }
            ShapeObject::Group(group) => {
                for child in &group.children {
                    shape(child, out);
                }
                if let Some(c) = &group.caption {
                    caption(c, out);
                }
            }
            other => {
                if let Some(d) = other.drawing() {
                    drawing(d, out);
                }
            }
        }
    }

    fn controls<'a>(controls: &'a [Control], out: &mut Vec<&'a Field>) {
        for control in controls {
            match control {
                Control::Picture(pic) => {
                    if let Some(c) = &pic.caption {
                        caption(c, out);
                    }
                }
                Control::Shape(s) => shape(s, out),
                Control::Table(table) => {
                    for cell in &table.cells {
                        paragraphs(&cell.paragraphs, out);
                    }
                    if let Some(c) = &table.caption {
                        caption(c, out);
                    }
                }
                Control::Header(h) => paragraphs(&h.paragraphs, out),
                Control::Footer(f) => paragraphs(&f.paragraphs, out),
                Control::Footnote(f) => paragraphs(&f.paragraphs, out),
                Control::Endnote(e) => paragraphs(&e.paragraphs, out),
                Control::HiddenComment(c) => paragraphs(&c.paragraphs, out),
                Control::SectionDef(sd) => master_pages(&sd.master_pages, out),
                Control::Field(field) if is_memo_field(field) => out.push(field),
                _ => {}
            }
        }
    }
}

/// 파서가 구역 끝에서 읽어 둔 메모 본문을 번호가 같은 메모 필드로 옮긴다.
///
/// 짝을 못 찾은 블록(앵커가 지워진 메모 등)은 원래 구역에 그대로 남겨 저장 때 다시 쓴다.
pub fn attach_memo_lists(sections: &mut [Section]) {
    let mut pending: Vec<(usize, MemoList)> = Vec::new();
    for (si, section) in sections.iter_mut().enumerate() {
        for list in section.memo_lists.drain(..) {
            pending.push((si, list));
        }
    }
    if pending.is_empty() {
        return;
    }
    for_each_field_mut(sections, &mut |field| {
        if !is_memo_field(field) || !field.memo_paragraphs.is_empty() {
            return;
        }
        let id = memo_id(field);
        if let Some(pos) = pending.iter().position(|(_, (mid, _))| *mid == id) {
            let (_, (_, paragraphs)) = pending.remove(pos);
            field.memo_paragraphs = paragraphs;
        }
    });
    for (si, list) in pending {
        sections[si].memo_lists.push(list);
    }
}

/// 저장할 메모 블록: 앵커가 있는 메모(번호 순) + 앵커 없이 남은 블록.
pub fn document_memo_lists(sections: &[Section]) -> Vec<MemoList> {
    let mut lists: Vec<MemoList> = collect_memo_fields(sections)
        .into_iter()
        .filter(|f| !f.memo_paragraphs.is_empty())
        .map(|f| (memo_id(f), f.memo_paragraphs.clone()))
        .collect();
    for section in sections {
        lists.extend(section.memo_lists.iter().cloned());
    }
    lists.sort_by_key(|(id, _)| *id);
    lists
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::control::FieldType;

    fn memo_field(id: u32) -> Field {
        Field {
            field_type: FieldType::Unknown,
            command: build_memo_command(id, (31282568u64 << 32) | 3296623872, "검토자"),
            memo_index: id,
            ..Default::default()
        }
    }

    #[test]
    fn command_round_trip() {
        let f = memo_field(7);
        assert!(f
            .command
            .starts_with("MEMO/65535/7/3296623872/31282568/검토자/"));
        assert!(is_memo_field(&f));
        assert_eq!(memo_id(&f), 7);
        assert_eq!(memo_author(&f), Some("검토자"));
        assert_eq!(memo_filetime(&f), Some((31282568u64 << 32) | 3296623872));
    }

    #[test]
    fn memo_id_falls_back_to_command() {
        let mut f = memo_field(9);
        f.memo_index = 0;
        assert_eq!(memo_id(&f), 9);
        f.command = "CLICKHERE/x".into();
        assert!(!is_memo_field(&f));
    }

    #[test]
    fn attach_moves_bodies_and_keeps_orphans() {
        let mut para = Paragraph::new_empty();
        para.controls.push(Control::Field(memo_field(1)));
        let body = |t: &str| {
            let mut p = Paragraph::new_empty();
            p.text = t.into();
            vec![p]
        };
        let mut sections = vec![Section {
            paragraphs: vec![para],
            memo_lists: vec![(1, body("하나")), (5, body("고아"))],
            ..Default::default()
        }];
        attach_memo_lists(&mut sections);
        let fields = collect_memo_fields(&sections);
        assert_eq!(memo_plain_text(&fields[0].memo_paragraphs), "하나");
        assert_eq!(sections[0].memo_lists.len(), 1);
        let lists = document_memo_lists(&sections);
        assert_eq!(
            lists.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1, 5]
        );
    }
}

/// 실문서 왕복: `HWP_MEMO_SAMPLE=<메모 있는 .hwp> cargo test --lib memo_real_doc -- --ignored`
#[cfg(test)]
mod real_doc {
    use super::*;

    #[test]
    #[ignore]
    fn memo_real_doc_round_trip() {
        let Ok(path) = std::env::var("HWP_MEMO_SAMPLE") else {
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        let mut doc = crate::parser::parse_document(&bytes).unwrap();
        let before: Vec<(u32, String)> = collect_memo_fields(&doc.sections)
            .iter()
            .map(|f| (memo_id(f), memo_plain_text(&f.memo_paragraphs)))
            .collect();
        eprintln!(
            "memos={} orphans={}",
            before.len(),
            doc.sections
                .iter()
                .map(|s| s.memo_lists.len())
                .sum::<usize>()
        );
        assert!(
            before.iter().all(|(_, t)| !t.is_empty()),
            "모든 메모에 본문이 붙어야 한다"
        );
        let paras_before = doc.sections[0].paragraphs.len();

        // 편집된 것처럼 — 모델에서 다시 직렬화
        for s in &mut doc.sections {
            s.raw_stream = None;
        }
        let out = crate::serializer::serialize_document(&doc).unwrap();
        if let Ok(p) = std::env::var("HWP_MEMO_OUT") {
            std::fs::write(p, &out).unwrap();
        }
        let again = crate::parser::parse_document(&out).unwrap();
        let after: Vec<(u32, String)> = collect_memo_fields(&again.sections)
            .iter()
            .map(|f| (memo_id(f), memo_plain_text(&f.memo_paragraphs)))
            .collect();
        assert_eq!(before, after);
        assert_eq!(
            again.sections[0].paragraphs.len(),
            paras_before,
            "뿌리 문단이 늘면 안 된다"
        );
    }
}
