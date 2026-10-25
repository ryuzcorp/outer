//! The billing tools (spec §8.2, §14.2): `upgrade` mints a Checkout
//! Session for the caller's account, `manage_billing` mints a Customer
//! Portal session for an existing subscriber. Both return a URL the human
//! opens; the plan itself changes only through webhooks.

use serde_json::{json, Value};

use super::{authed, internal, ok, Ctx, NoArgs};
use crate::mcp::error::ToolError;

/// `upgrade`: one Pro Checkout Session for the caller (spec §14.2). The
/// response URL is live the moment it is minted — the human pays $4 with
/// the launch coupon in test mode (or $8 without it), tax calculated, and
/// the next tool call sees Pro.
pub async fn upgrade(_input: NoArgs, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    match ctx.state.billing.checkout_url(&caller.account_id, &caller.email).await {
        None => Err(ToolError::internal(
            "billing is not enabled on this host; upgrade from the console plan card once it is",
        )),
        Some(Err(err)) => Err(internal(err.context("creating the checkout session"))),
        Some(Ok(session)) => Ok(ok(
            format!(
                "Open {} to upgrade to Outer Pro (launch price $4/month with the coupon, then $8/month; tax calculated at checkout).",
                session.url
            ),
            json!({"checkout_url": session.url}),
        )),
    }
}

/// `manage_billing`: one Customer Portal session for a subscriber (update
/// the card, cancel). Accounts without a Stripe customer yet have nothing
/// to manage: `upgrade` first.
pub async fn manage_billing(_input: NoArgs, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    let customer: Option<String> =
        sqlx::query_scalar("SELECT stripeCustomerId FROM user WHERE id = ?1")
            .bind(&caller.account_id)
            .fetch_optional(&ctx.state.pool)
            .await
            .map_err(internal)?;
    let Some(customer) = customer else {
        return Err(ToolError::invalid_argument(
            "no billing history on this account yet; call upgrade to start Pro first",
        ));
    };
    match ctx.state.billing.portal_url(&customer).await {
        None => Err(ToolError::internal(
            "billing is not enabled on this host; manage billing from the console plan card once it is",
        )),
        Some(Err(err)) => Err(internal(err.context("creating the portal session"))),
        Some(Ok(session)) => Ok(ok(
            format!("Open {} to manage Outer Pro billing.", session.url),
            json!({"portal_url": session.url}),
        )),
    }
}
