//! Preserve the pre-poll T1/direct graph for non-suspending (flat) dispatch.
//! T0 has different poll grouping, so using it for every entry changes native time.
use super::*;

pub struct Flat {
    pub t1: String,
    pub direct: Option<String>,
    pub fields: Vec<(ir::FieldRef, ir::Access)>,
}

pub fn table(sigs: &HashMap<(String, u32), raise_emit::DirectSig>) -> HashMap<u32, Vec<(String, raise_emit::DirectSig)>> {
    let mut rows: HashMap<u32, Vec<(String, raise_emit::DirectSig)>> = HashMap::new();
    for ((seg, entry), sig) in sigs {
        rows.entry(*entry).or_default().push((seg.clone(), sig.clone()));
    }
    rows
}

pub fn emit<'a>(jobs: &[(String, &lift::Func, String, Option<String>)],
    image: &impl Fn(&Option<String>) -> lift::Image<'a>,
    sigs: &HashMap<(String, u32), raise_emit::DirectSig>,
) -> HashMap<String, Flat> {
    let boundary = std::env::var_os("MKDS_BOUNDARY");
    unsafe { std::env::set_var("MKDS_POLL_DIRECT", "0"); }
    unsafe { std::env::set_var("MKDS_BOUNDARY", "0"); }
    raise_emit::set_direct(table(sigs));
    raise_emit::set_boundary(HashMap::new());
    let mut rows = HashMap::new();
    for (seg, f, id, src) in jobs {
        let img = image(src);
        raise_emit::set_current(seg, sigs.get(&(seg.clone(), f.entry)).cloned());
        let code = with_notes9(seg, f.entry, |n| raise_emit::emit_t1_annotated(f, id, Some(&img), n));
        let fields = ir::take_field_uses();
        let direct = raise_emit::take_direct();
        let _ = raise_emit::take_facts();
        let _ = raise_emit::take_types();
        let _ = raise_emit::take_dump();
        if let Ok(t1) = code {
            rows.insert(id.clone(), Flat {
                t1: t1.replacen(&format!("pub fn f_{id}_t1("), &format!("pub fn f_{id}_flat_t1("), 1),
                direct, fields,
            });
        }
    }
    unsafe { std::env::set_var("MKDS_POLL_DIRECT", "1"); }
    unsafe { match boundary {
        Some(v) => std::env::set_var("MKDS_BOUNDARY", v),
        None => std::env::remove_var("MKDS_BOUNDARY"),
    } }
    rows
}

pub fn attach(f: &mut Lifted, flat: Flat) {
    if let Ok(t1) = &mut f.t1 {
        let old = format!("if !m.can_suspend() {{ return f_{}(c, m, pc); }}", f.id);
        let new = format!("if !m.can_suspend() {{ return f_{}_flat_t1(c, m, pc); }}", f.id);
        *t1 = t1.replacen(&old, &new, 1);
        t1.push_str(&flat.t1);
        f.fields.extend(flat.fields);
        if let Some(direct) = flat.direct { f.direct.get_or_insert_with(String::new).push_str(&direct); }
    }
}
