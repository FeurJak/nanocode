use std::cmp::Ordering;

pub(crate) fn sort_by<T>(v: &mut [T], mut cmp: impl FnMut(&T, &T) -> Ordering) {
    if v.len() < 2 {
        return;
    }
    let mut order: Vec<usize> = (0..v.len()).collect();
    positions(&mut order, &mut |a, b| cmp(&v[a], &v[b]));
    permute(v, &mut order);
}

pub(crate) fn sort_by_key<T, K: Ord>(v: &mut [T], mut key: impl FnMut(&T) -> K) {
    sort_by(v, |a, b| key(a).cmp(&key(b)));
}

pub(crate) fn sort<T: Ord>(v: &mut [T]) {
    sort_by(v, T::cmp);
}

#[inline(never)]
fn positions(order: &mut [usize], cmp: &mut dyn FnMut(usize, usize) -> Ordering) {
    order.sort_by(|&a, &b| cmp(a, b));
}

fn permute<T>(v: &mut [T], order: &mut [usize]) {
    for i in 0..order.len() {
        let mut j = i;
        while order[j] != i {
            let k = order[j];
            v.swap(j, k);
            order[j] = j;
            j = k;
        }
        order[j] = j;
    }
}
