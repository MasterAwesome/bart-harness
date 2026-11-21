use serde_json::json;

use super::CellReview;

pub(super) async fn approve(
    review: &mut CellReview<'_, '_>,
    helper: &str,
    command: &str,
    reason: &str,
) -> Result<crate::approval::Outside, String> {
    let context = review.context;
    let gate = review
        .gate
        .as_deref_mut()
        .expect("a jailed session reviews every cell");
    let stopped = || {
        context
            .interpreter
            .stopped
            .load(std::sync::atomic::Ordering::Acquire)
    };
    if gate.stopped || stopped() {
        return Err("not run: session stopped".into());
    }
    let prompt = format!(
        "Run outside the sandbox ({helper}) in {}\nCommand: {}\nReason: {}\n",
        json!(context.cwd),
        json!(command),
        json!(reason)
    );
    let outside = gate
        .check_outside(
        review.id,
        json!({"kind": "outside_sandbox", "helper": helper, "command": command, "reason": reason, "cwd": context.cwd}),
        &prompt,
    )
    .await?;
    if stopped() {
        return Err("not run: session stopped".into());
    }
    Ok(outside)
}

#[cfg(test)]
mod tests;
