use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::path::Path;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::backup::BackupSession;
use crate::subscription_identity::number_from_id;
use crate::subscription_parser::parse_subscription;
use crate::subscriptions::{Subscription, SubscriptionStore};
use crate::terminal::{error, info, menu_item, read_input, success, title, GREEN, RED, RESET};
use crate::update_state::{PreparedSubscription, RunState};
use crate::update_subscription::baseline_path;

type AppResult<T> = Result<T, Box<dyn Error>>;
pub(crate) const BLACKLIST_FILE: &str = "/opt/etc/xkeen-sub/blacklist.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub(crate) struct BlacklistStore {
    #[serde(default)]
    pub(crate) entries: Vec<BlacklistEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct BlacklistEntry {
    pub(crate) subscription_id: String,
    pub(crate) tag: String,
}

pub(crate) fn load() -> AppResult<BlacklistStore> {
    let path = Path::new(BLACKLIST_FILE);
    if !path.exists() { return Ok(BlacklistStore::default()); }
    let text = fs::read_to_string(path)?;
    let mut store: BlacklistStore = serde_json::from_str(&text)
        .map_err(|e| format!("Файл чёрного списка повреждён: {e}"))?;
    store.normalize();
    Ok(store)
}

impl BlacklistStore {
    fn normalize(&mut self) {
        let mut seen = HashSet::new();
        self.entries.retain(|e| !e.subscription_id.trim().is_empty()
            && !e.tag.trim().is_empty()
            && seen.insert((e.subscription_id.clone(), e.tag.clone())));
        self.entries.sort_by(|a,b| a.subscription_id.cmp(&b.subscription_id).then(a.tag.cmp(&b.tag)));
    }
    pub(crate) fn is_blocked(&self, id: &str, tag: &str) -> bool {
        self.entries.iter().any(|e| e.subscription_id == id && e.tag == tag)
    }
    pub(crate) fn blocked_full_tags(&self, subscriptions: &[Subscription]) -> HashSet<String> {
        self.entries.iter().filter_map(|entry| subscriptions.iter().find(|sub| sub.id == entry.subscription_id).map(|sub| format!("{}{}", sub.prefix, entry.tag))).collect()
    }
    pub(crate) fn filter_outbounds(&self, subscriptions: &[Subscription], outbounds: &mut Vec<Value>) -> AppResult<usize> {
        let before = outbounds.len();
        outbounds.retain(|outbound| {
            let Some(tag) = outbound.get("tag").and_then(Value::as_str) else { return true; };
            let Some(sub) = subscriptions.iter().filter(|s| tag.starts_with(&s.prefix))
                .max_by_key(|s| s.prefix.len()) else { return true; };
            let original = tag.strip_prefix(&sub.prefix).unwrap_or(tag);
            !self.is_blocked(&sub.id, original)
        });
        Ok(before - outbounds.len())
    }
    pub(crate) fn remap_after_deletion(&mut self, removed: &str, mapping: &[(String,String)]) {
        let ids: HashMap<&str,&str> = mapping.iter().map(|(a,b)|(a.as_str(),b.as_str())).collect();
        self.entries.retain_mut(|e| {
            if e.subscription_id == removed { return false; }
            let Some(id) = ids.get(e.subscription_id.as_str()) else { return false; };
            e.subscription_id = (*id).to_owned(); true
        });
        self.normalize();
    }
    /// Removes stale entries only for subscriptions whose current source tags
    /// were parsed completely. Subscriptions absent from observed_tags are untouched.
    pub(crate) fn prune_missing_tags(&mut self, observed_tags: &[(String, Vec<String>)]) -> usize {
        let observed: HashMap<&str, HashSet<&str>> = observed_tags.iter()
            .filter(|(_, tags)| !tags.is_empty())
            .map(|(id, tags)| (id.as_str(), tags.iter().map(String::as_str).collect()))
            .collect();
        let before = self.entries.len();
        self.entries.retain(|entry| {
            observed.get(entry.subscription_id.as_str())
                .map(|tags| tags.contains(entry.tag.as_str()))
                .unwrap_or(true)
        });
        self.normalize();
        before - self.entries.len()
    }

    fn toggle(&mut self, id: &str, tag: &str) -> bool {
        if let Some(i) = self.entries.iter().position(|e| e.subscription_id == id && e.tag == tag) {
            self.entries.remove(i); false
        } else {
            self.entries.push(BlacklistEntry { subscription_id:id.to_owned(), tag:tag.to_owned() });
            self.normalize(); true
        }
    }
}

pub(crate) fn run_for_subscription(store: &SubscriptionStore, index: usize) -> AppResult<()> {
    let sub = store.subscriptions.get(index).ok_or("Подписка не найдена.")?;
    let path = baseline_path(sub)?;
    if !path.is_file() { info("Baseline отсутствует. Сначала обновите подписку."); return Ok(()); }
    let parsed = parse_subscription(&fs::read(path)?)?;
    let mut blacklist = load()?;
    loop {
        println!(); title(&format!("ЧЁРНЫЙ СПИСОК — {}", sub.prefix));
        for (i, outbound) in parsed.outbounds.iter().enumerate() {
            let tag = outbound["tag"].as_str().ok_or("Подключение без Tag.")?;
            let status = if blacklist.is_blocked(&sub.id,tag) {
                format!("{RED}[ЗАБЛОКИРОВАНО]{RESET}")
            } else {
                format!("{GREEN}[РАЗРЕШЕНО]{RESET}")
            };
            menu_item(&(i+1).to_string(), &format!("{tag} {status}"));
        }
        menu_item("0", "Назад");
        let choice = read_input("Выберите подключение: ")?;
        if choice == "0" { return Ok(()); }
        let i = match choice.parse::<usize>() { Ok(n) if n>0 && n<=parsed.outbounds.len()=>n-1, _=>{error("Некорректный номер.");continue;} };
        let tag = parsed.outbounds[i]["tag"].as_str().ok_or("Подключение без Tag.")?.to_owned();
        if !blacklist.is_blocked(&sub.id, &tag) && store.subscriptions.len() == 1 {
            let available = parsed.outbounds.iter().filter(|outbound| {
                outbound.get("tag").and_then(Value::as_str)
                    .map(|raw_tag| !blacklist.is_blocked(&sub.id, raw_tag))
                    .unwrap_or(false)
            }).count();
            if available <= 1 {
                error("Нельзя заблокировать последнее подключение единственной подписки. Хотя бы одно подключение должно оставаться доступным.");
                continue;
            }
        }
        let blocked = blacklist.toggle(&sub.id,&tag);
        apply_change(store, blacklist.clone())?;
        success(if blocked {"Подключение добавлено в чёрный список."} else {"Блокировка снята."});
        blacklist = load()?;
    }
}

pub(crate) fn run_menu() -> AppResult<bool> {
    loop {
        let store = crate::subscriptions::load_subscriptions()?;
        if store.subscriptions.is_empty() {
            println!();
            title("XKEEN-SUB — ЧЁРНЫЙ СПИСОК");
            info("Подписок пока нет. Сначала добавьте подписку.");
            return Ok(false);
        }

        let blacklist = load()?;
        println!();
        title("XKEEN-SUB — ЧЁРНЫЙ СПИСОК");
        for (index, subscription) in store.subscriptions.iter().enumerate() {
            menu_item(&(index + 1).to_string(), &subscription.prefix);
        }

        let mut next = store.subscriptions.len() + 1;
        let blocked_menu_number = if blacklist.entries.is_empty() {
            None
        } else {
            menu_item(&next.to_string(), "Просмотреть все заблокированные подключения");
            let number = next;
            next += 1;
            Some(number)
        };
        menu_item(&next.to_string(), "Назад");
        menu_item("0", "Выход");

        let choice = read_input("Выберите подписку: ")?;
        if choice == next.to_string() {
            return Ok(false);
        }
        if choice == "0" {
            return Ok(true);
        }
        if blocked_menu_number.map(|number| choice == number.to_string()).unwrap_or(false) {
            run_blocked_entries_menu(&store)?;
            continue;
        }

        let index = match choice.parse::<usize>() {
            Ok(number) if number > 0 && number <= store.subscriptions.len() => number - 1,
            _ => {
                error("Некорректный номер подписки.");
                continue;
            }
        };
        run_for_subscription(&store, index)?;
    }
}

fn run_blocked_entries_menu(store: &SubscriptionStore) -> AppResult<()> {
    loop {
        let mut blacklist = load()?;
        println!();
        title("XKEEN-SUB — ЗАБЛОКИРОВАННЫЕ ПОДКЛЮЧЕНИЯ");
        if blacklist.entries.is_empty() {
            info("Заблокированных подключений нет.");
            return Ok(());
        }
        for (index, entry) in blacklist.entries.iter().enumerate() {
            let prefix = store.subscriptions.iter().find(|subscription| subscription.id == entry.subscription_id)
                .map(|subscription| subscription.prefix.as_str())
                .unwrap_or(entry.subscription_id.as_str());
            menu_item(&(index + 1).to_string(), &format!("{prefix}{}", entry.tag));
        }
        menu_item("0", "Назад");

        let choice = read_input("Выберите запись для снятия блокировки: ")?;
        if choice == "0" {
            return Ok(());
        }
        let index = match choice.parse::<usize>() {
            Ok(number) if number > 0 && number <= blacklist.entries.len() => number - 1,
            _ => {
                error("Некорректный номер.");
                continue;
            }
        };
        blacklist.entries.remove(index);
        apply_change(store, blacklist)?;
        success("Блокировка снята.");
    }
}

fn apply_change(store: &SubscriptionStore, blacklist: BlacklistStore) -> AppResult<()> {
    let mut backup=BackupSession::start("Изменение чёрного списка")?;
    backup.set_subscriptions(store.subscriptions.iter().map(|s|s.id.clone()).collect());
    let result=(|| -> AppResult<()> {
        let mut state=RunState::default();
        for sub in &store.subscriptions {
            let _number=number_from_id(&sub.id).ok_or("Некорректный ID подписки.")?;
            let path=baseline_path(sub)?;
            // A saved baseline marks a subscription that has been applied before.
            // Rebuild it even when every current outbound is blacklisted and therefore
            // no longer appears in 04_outbound; otherwise unblocking could not restore it.
            if !path.is_file() { continue; }
            let parsed=parse_subscription(&fs::read(&path).map_err(|e|format!("Не удалось прочитать baseline {}: {e}",path.display()))?)?;
            let group=parsed.outbounds.into_iter().map(|mut o| { let raw=o["tag"].as_str().unwrap_or("").to_owned(); o["tag"]=Value::String(format!("{}{}",sub.prefix,raw)); o }).collect();
            state.prepared.push(PreparedSubscription{id:sub.id.clone(),outbounds:group});
        }
        let applied=crate::update_configuration::rebuild_configuration_with_blacklist(&store.subscriptions,&state,&mut backup,store,&blacklist)?;
        if !applied { return Err("Проверка Xray не пройдена; изменения отменены.".into()); }
        Ok(())
    })();
    if let Err(ref e)=result { if backup.is_running(){backup.set_outcome("ОШИБКА ЧЁРНОГО СПИСКА",&format!("{e}"),true);} }
    let log=backup.finish();
    if let Err(e)=log{return Err(format!("Не удалось завершить журнал: {e}").into());}
    result
}

#[cfg(test)]
mod tests {
    use super::{BlacklistEntry,BlacklistStore};
    use crate::subscriptions::Subscription;
    use serde_json::json;
    #[test]
    fn blacklist_survives_subscription_rename() {
        let store=BlacklistStore{entries:vec![BlacklistEntry{subscription_id:"sub1".into(),tag:"node".into()}]};
        let sub=Subscription{id:"sub1".into(),name:Some("New".into()),url:"https://example.test".into(),prefix:"sub1--New--".into()};
        let mut outs=vec![json!({"tag":"sub1--New--node"}),json!({"tag":"sub1--New--keep"})];
        assert_eq!(store.filter_outbounds(&[sub],&mut outs).unwrap(),1);
        assert_eq!(outs[0]["tag"],"sub1--New--keep");
    }
    #[test]
    fn pruning_removes_only_missing_tags_for_observed_subscriptions() {
        let mut store=BlacklistStore{entries:vec![
            BlacklistEntry{subscription_id:"sub1".into(),tag:"keep".into()},
            BlacklistEntry{subscription_id:"sub1".into(),tag:"gone".into()},
            BlacklistEntry{subscription_id:"sub2".into(),tag:"untouched".into()},
        ]};
        let removed=store.prune_missing_tags(&[("sub1".into(),vec!["keep".into(),"new".into()])]);
        assert_eq!(removed,1);
        assert_eq!(store.entries,vec![
            BlacklistEntry{subscription_id:"sub1".into(),tag:"keep".into()},
            BlacklistEntry{subscription_id:"sub2".into(),tag:"untouched".into()},
        ]);
    }

    #[test]
    fn pruning_does_not_clear_entries_for_empty_or_unobserved_subscriptions() {
        let mut store=BlacklistStore{entries:vec![
            BlacklistEntry{subscription_id:"sub1".into(),tag:"blocked".into()},
        ]};
        assert_eq!(store.prune_missing_tags(&[]),0);
        assert_eq!(store.prune_missing_tags(&[("sub1".into(),Vec::new())]),0);
        assert_eq!(store.entries.len(),1);
    }

    #[test]
    fn deleting_subscription_remaps_remaining_blacklist_ids() {
        let mut store=BlacklistStore{entries:vec![
            BlacklistEntry{subscription_id:"sub1".into(),tag:"a".into()},
            BlacklistEntry{subscription_id:"sub2".into(),tag:"b".into()},
            BlacklistEntry{subscription_id:"sub3".into(),tag:"c".into()}]};
        store.remap_after_deletion("sub2",&[("sub1".into(),"sub1".into()),("sub3".into(),"sub2".into())]);
        assert_eq!(store.entries,vec![
            BlacklistEntry{subscription_id:"sub1".into(),tag:"a".into()},
            BlacklistEntry{subscription_id:"sub2".into(),tag:"c".into()}]);
    }
}
