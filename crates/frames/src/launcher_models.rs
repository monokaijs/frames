//! Keep device cards alive while discovery refreshes, including search results.
use super::DeviceRow;
use slint::{FilterModel, Model, ModelRc, SharedString, VecModel};
use std::{cell::RefCell, rc::Rc};

pub fn device_filter() -> impl Fn(ModelRc<DeviceRow>, SharedString) -> ModelRc<DeviceRow> {
    let cache = RefCell::new(Vec::<(ModelRc<DeviceRow>, String, ModelRc<DeviceRow>)>::new());
    move |source, query| {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return source;
        }
        let mut cache = cache.borrow_mut();
        let slot = cache.iter().position(|entry| entry.0 == source);
        if let Some(index) = slot
            && cache[index].1 == query
        {
            return cache[index].2.clone();
        }
        let needle = query.clone();
        let filtered: ModelRc<DeviceRow> =
            Rc::new(FilterModel::new(source.clone(), move |row: &DeviceRow| {
                format!("{} {} {}", row.name, row.address, row.fingerprint)
                    .to_lowercase()
                    .contains(&needle)
            }))
            .into();
        let entry = (source, query, filtered.clone());
        if let Some(index) = slot {
            cache[index] = entry;
        } else {
            cache.push(entry);
        }
        filtered
    }
}

/// Insert/remove individual rows instead of resetting every card when a device
/// arrives. A refresh between pointer-down and pointer-up must keep its target.
pub fn refresh_rows<T: Clone + PartialEq + 'static, K: PartialEq>(
    current: ModelRc<T>,
    rows: Vec<T>,
    key: impl Fn(&T) -> K,
    replace: impl FnOnce(ModelRc<T>),
) {
    let Some(list) = current.as_any().downcast_ref::<VecModel<T>>() else {
        replace(Rc::new(VecModel::from(rows)).into());
        return;
    };
    for index in (0..list.row_count()).rev() {
        if !rows
            .iter()
            .any(|row| key(row) == key(&list.row_data(index).unwrap()))
        {
            list.remove(index);
        }
    }
    for (index, row) in rows.into_iter().enumerate() {
        if list
            .row_data(index)
            .as_ref()
            .is_some_and(|old| key(old) == key(&row))
        {
            if list.row_data(index).as_ref() != Some(&row) {
                list.set_row_data(index, row);
            }
        } else {
            if let Some(old) =
                (index..list.row_count()).find(|&i| key(&list.row_data(i).unwrap()) == key(&row))
            {
                list.remove(old);
            }
            list.insert(index, row);
        }
    }
}

pub fn device_key(row: &DeviceRow) -> SharedString {
    if row.fingerprint.is_empty() {
        row.address.clone()
    } else {
        row.fingerprint.clone()
    }
}
