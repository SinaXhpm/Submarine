//! Which of two versions is newer: an update, the same, or a newer one already
//! installed. Numbers part by part; a pre-release (0.5.0-beta.6) comes before
//! its release; build metadata (+...) doesn't count.
use std::cmp::Ordering;

fn split(v: &str) -> (Vec<u64>, Option<&str>) {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let v = v.split('+').next().unwrap_or("");
    let (core, pre) = match v.find('-') {
        Some(i) => (&v[..i], Some(&v[i + 1..])),
        None => (v, None),
    };
    (core.split('.').map(|p| p.trim().parse().unwrap_or(0)).collect(), pre)
}

fn compare_pre(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.split('.'), b.split('.'));
    loop {
        let order = match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                _ => x.cmp(y),
            },
        };
        if order != Ordering::Equal {
            return order;
        }
    }
}

pub fn compare(a: &str, b: &str) -> Ordering {
    let ((a_core, a_pre), (b_core, b_pre)) = (split(a), split(b));
    for i in 0..a_core.len().max(b_core.len()) {
        let order = a_core.get(i).unwrap_or(&0).cmp(b_core.get(i).unwrap_or(&0));
        if order != Ordering::Equal {
            return order;
        }
    }
    match (a_pre, b_pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => compare_pre(x, y),
    }
}

#[cfg(test)]
mod tests {
    use super::compare;
    use std::cmp::Ordering::*;

    #[test]
    fn order() {
        assert_eq!(compare("0.5.0", "0.4.2"), Greater);
        assert_eq!(compare("0.5.0", "0.5"), Equal);
        assert_eq!(compare("0.10.0", "0.9.9"), Greater);
        assert_eq!(compare("0.5.0", "0.5.0-beta.6"), Greater);
        assert_eq!(compare("0.5.0-beta.10", "0.5.0-beta.6"), Greater);
        assert_eq!(compare("0.5.0-beta.6", "0.5.0-rc.1"), Less);
        assert_eq!(compare("v1.2.3+build.7", "1.2.3"), Equal);
        assert_eq!(compare("", "0.0.1"), Less);
    }
}
