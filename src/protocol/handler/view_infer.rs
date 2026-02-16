//! View inference stack — prevents infinite recursion when inferring view schemas.

use std::cell::RefCell;
use std::future::Future;

tokio::task_local! {
    pub(super) static VIEW_INFERENCE_STACK: RefCell<Vec<String>>;
}

pub(super) const MAX_VIEW_INFERENCE_DEPTH: usize = 64;

pub(super) async fn with_view_inference_stack<T>(future: impl Future<Output = T>) -> T {
    if VIEW_INFERENCE_STACK.try_with(|_| ()).is_ok() {
        future.await
    } else {
        VIEW_INFERENCE_STACK
            .scope(RefCell::new(Vec::new()), future)
            .await
    }
}

pub(super) struct ViewInferenceGuard {
    view_name: String,
}

impl ViewInferenceGuard {
    pub(super) fn push(view_name: String) -> Option<Self> {
        VIEW_INFERENCE_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            if stack.contains(&view_name) || stack.len() >= MAX_VIEW_INFERENCE_DEPTH {
                return None;
            }
            stack.push(view_name.clone());
            Some(Self { view_name })
        })
    }
}

impl Drop for ViewInferenceGuard {
    fn drop(&mut self) {
        let view_name = &self.view_name;
        let _ = VIEW_INFERENCE_STACK.try_with(|stack| {
            let mut stack = stack.borrow_mut();
            if stack.last().map(|s| s == view_name).unwrap_or(false) {
                stack.pop();
            } else if let Some(pos) = stack.iter().rposition(|s| s == view_name) {
                stack.remove(pos);
            }
        });
    }
}
