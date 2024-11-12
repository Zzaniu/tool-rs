use anyhow::{anyhow, Result as AnyResult};
pub use cookie_store;
pub use reqwest;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Client;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

const STORE_COOKIE_PATH: &str = "spider.cookie";
const USER_AGENT_NAME: &str = "User-Agent";
const USER_AGENT_VALUE:&str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/114.0.0.0 Safari/537.36";

// Rust runtime 在离开作用域时自动释放内存
// Rc 在单线程下能保证内存正确释放, 多线程下无法保证引用技术原子性(更加不能保证数据的原子性), 导致内存不能正确释放(其实就是计数为 0 时, 使离开作用域)
// Arc 只保证引用计数的原子性(内存能够正确释放), 不保证数据的原子性.
// Mutex 可以保证数据的原子性, 但是需要配合 Arc 来达到内存正确释放
// Cell 是一个内部可变的智能指针(数据得实现了 Copy). 通过 get 获取, set 设置. 无性能损耗
// RefCell 同 Cell, 数据未实现 Copy
pub struct Session {
    cookie: Arc<reqwest_cookie_store::CookieStoreMutex>,
    client: Client,
    store_cookie_path: &'static str,
}

impl Default for Session {
    /// 创建`Session`, 会自动管理`cookie`, 结束时会自动保存`cookie`到当前目录的`spider.cookie`文件
    fn default() -> Self {
        let cookie = reqwest_cookie_store::CookieStoreMutex::default();
        let cookie = Arc::new(cookie);
        let client = Client::builder()
            .timeout(Duration::from_secs(15)) // 默认 15 S
            .default_headers(get_head_map())
            .cookie_provider(cookie.clone())
            .build()
            .unwrap();
        Self {
            cookie,
            client,
            store_cookie_path: STORE_COOKIE_PATH,
        }
    }
}

impl Session {
    pub fn new_with_redirect_policy(policy: reqwest::redirect::Policy) -> Self {
        let cookie = reqwest_cookie_store::CookieStoreMutex::default();
        let cookie = Arc::new(cookie);
        let client = Client::builder()
            .default_headers(get_head_map())
            .cookie_provider(cookie.clone())
            .redirect(policy)
            .build()
            .unwrap();
        Self {
            cookie,
            client,
            store_cookie_path: STORE_COOKIE_PATH,
        }
    }
    /// 创建`Session`时携带本地的`cookie`进行创建, 如果`cookie`不存在, 则调用`Session::new`进行创建
    pub fn new_with_cookie(
        store_cookie_path: &'static str,
        policy: Option<reqwest::redirect::Policy>,
    ) -> Self {
        let info = std::fs::read(store_cookie_path).unwrap_or_default();
        if info.is_empty() {
            let mut s = Self::default();
            if let Some(policy) = policy {
                s = Self::new_with_redirect_policy(policy);
            }
            s.store_cookie_path = store_cookie_path;
            return s;
        }

        let cookie_store =
            cookie_store::CookieStore::load_all(&info[..], |cookie| serde_json::from_str(cookie))
                .unwrap();
        let cookie = Arc::new(reqwest_cookie_store::CookieStoreMutex::new(cookie_store));
        let mut builder = Client::builder()
            .default_headers(get_head_map())
            .cookie_provider(cookie.clone());
        if let Some(policy) = policy {
            builder = builder.redirect(policy);
        }
        let client = builder.build().unwrap();
        Self {
            cookie,
            client,
            store_cookie_path,
        }
    }

    /// 获取 cookie
    pub fn gey_cookie(&self) -> cookie_store::CookieStore {
        let cookie = self.cookie.lock();
        cookie.unwrap().clone()
    }

    /// 加载`cookie`
    pub fn load_cookie(&self, cookie_store: cookie_store::CookieStore) {
        *self.cookie.lock().unwrap() = cookie_store;
    }

    /// 清除`cookie`
    pub fn clear_cookie(&self) {
        self.cookie.lock().unwrap().clear();
    }

    pub fn get_store_cookie_path(&self) -> &str {
        self.store_cookie_path
    }

    pub fn save_cookie(&self) -> AnyResult<()> {
        let cookie = self
            .cookie
            .lock()
            .map_err(|err| anyhow!("cookie lock error: {err:?}"))?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(self.store_cookie_path)?;
        cookie
            .save_incl_expired_and_nonpersistent(&mut f, serde_json::to_string)
            .map_err(|err| anyhow!("cookie save error: {err:?}"))?;
        Ok(())
    }
}

impl Deref for Session {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

pub fn get_head_map() -> HeaderMap {
    let mut header = HeaderMap::new();
    header.insert(
        USER_AGENT_NAME,
        HeaderValue::from_str(USER_AGENT_VALUE).unwrap(),
    );
    header
}
