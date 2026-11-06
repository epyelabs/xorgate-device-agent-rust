//! gst-launch's `_()` strings: looked up in the `gstreamer-1.0` gettext
//! domain (libgstreamer binds it in `gst_init`), so under a translated locale
//! (the Pi's `en_GB.UTF-8`, say) the host prints exactly what gst-launch 1.22
//! prints. In the C/POSIX locale (the rig) `dgettext` returns the msgid.
//!
//! The msgids are copied byte for byte from `tools/gst-launch.c` @ 1.22.0
//! (`GST_TIME_FORMAT` expanded), and [`format`] is the subset of printf they
//! use: `%s`, `%d`, `%u`, zero-padded widths, `%%` and positional `%N$`.

use std::ffi::CStr;

const DOMAIN: &CStr = c"gstreamer-1.0";

extern "C" {
    /// glibc's libintl (part of libc on glibc targets).
    fn dgettext(domain: *const libc::c_char, msgid: *const libc::c_char) -> *mut libc::c_char;
}

/// `_(msgid)`.
#[allow(unsafe_code)]
pub fn tr(msgid: &'static CStr) -> String {
    // SAFETY: both arguments are NUL-terminated; dgettext returns either
    // `msgid` itself or a pointer into the loaded catalogue, valid for the
    // process lifetime, never freed by the caller.
    let p = unsafe { dgettext(DOMAIN.as_ptr(), msgid.as_ptr()) };
    if p.is_null() {
        return msgid.to_string_lossy().into_owned();
    }
    // SAFETY: non-null result of dgettext is a NUL-terminated string.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// One printf argument.
#[derive(Debug, Clone)]
pub enum Arg<'a> {
    S(&'a str),
    U(u64),
    D(i64),
}

/// printf(3) for the conversions gst-launch's messages use.
pub fn format(fmt: &str, args: &[Arg<'_>]) -> String {
    let mut out = String::with_capacity(fmt.len() + 16);
    let b = fmt.as_bytes();
    let mut i = 0;
    let mut next = 0usize;
    while i < b.len() {
        if b[i] != b'%' {
            let start = i;
            while i < b.len() && b[i] != b'%' {
                i += 1;
            }
            out.push_str(&fmt[start..i]);
            continue;
        }
        i += 1;
        if i < b.len() && b[i] == b'%' {
            out.push('%');
            i += 1;
            continue;
        }
        // %[N$][0][width]conv
        let spec_start = i;
        let mut n = 0usize;
        while i < b.len() && b[i].is_ascii_digit() {
            n = n * 10 + usize::from(b[i] - b'0');
            i += 1;
        }
        let mut idx = None;
        if i < b.len() && b[i] == b'$' {
            idx = Some(n.saturating_sub(1));
            i += 1;
        } else {
            i = spec_start;
        }
        let mut zero = false;
        if i < b.len() && b[i] == b'0' {
            zero = true;
            i += 1;
        }
        let mut width = 0usize;
        while i < b.len() && b[i].is_ascii_digit() {
            width = width * 10 + usize::from(b[i] - b'0');
            i += 1;
        }
        // Length modifiers the msgids could carry (`l`, `ll`, `z`, `G_GSIZE…`).
        while i < b.len() && matches!(b[i], b'l' | b'z' | b'h' | b'j' | b't') {
            i += 1;
        }
        let Some(&conv) = b.get(i) else { break };
        i += 1;
        let k = idx.unwrap_or_else(|| {
            let k = next;
            next += 1;
            k
        });
        let text = match (conv, args.get(k)) {
            (_, Some(Arg::S(s))) => (*s).to_string(),
            (_, Some(Arg::U(v))) => v.to_string(),
            (_, Some(Arg::D(v))) => v.to_string(),
            (_, None) => String::new(),
        };
        if text.len() < width {
            let pad = if zero && conv != b's' { '0' } else { ' ' };
            out.extend(std::iter::repeat_n(pad, width - text.len()));
        }
        out.push_str(&text);
    }
    out
}

/// `_(msgid)` then [`format`].
pub fn trf(msgid: &'static CStr, args: &[Arg<'_>]) -> String {
    format(&tr(msgid), args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printf_subset() {
        assert_eq!(
            format("Got EOS from element \"%s\".\n", &[Arg::S("pipeline0")]),
            "Got EOS from element \"pipeline0\".\n"
        );
        assert_eq!(
            format(
                "Execution ended after %u:%02u:%02u.%09u\n",
                &[Arg::U(0), Arg::U(0), Arg::U(2), Arg::U(2_935_751)]
            ),
            "Execution ended after 0:00:02.002935751\n"
        );
        assert_eq!(format("%2$s-%1$s %%", &[Arg::S("a"), Arg::S("b")]), "b-a %");
        assert_eq!(format("%d%%  \r", &[Arg::D(-5)]), "-5%  \r");
    }

    #[test]
    fn untranslated_in_the_c_locale() {
        // The test process never calls setlocale: the C locale returns msgids.
        assert_eq!(tr(c"Freeing pipeline ...\n"), "Freeing pipeline ...\n");
    }
}
