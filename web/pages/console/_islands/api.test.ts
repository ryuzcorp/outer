// The Export card's "live version" is the newest *successful* deploy —
// the default the download button fetches and the picker labels "live".
// A failed head deploy must not move the default (or the label) onto a
// broken tree.
import { describe, expect, test } from "bun:test";

import { liveDeploySha, type ConsoleDeploy } from "./api.ts";

function deploy(sha: string, status: string): ConsoleDeploy {
  return {
    deploy_id: `deploy-${sha}`,
    sha,
    status,
    findings_count: 0,
    created_at: 0,
  };
}

describe("liveDeploySha", () => {
  test("skips a failed head deploy for the newest success", () => {
    expect(
      liveDeploySha([
        deploy("bad", "failed"),
        deploy("good", "success"),
        deploy("old", "success"),
      ]),
    ).toBe("good");
  });

  test("null when nothing succeeded yet", () => {
    expect(
      liveDeploySha([deploy("a", "failed"), deploy("b", "blocked")]),
    ).toBeNull();
  });

  test("null when there are no deploys", () => {
    expect(liveDeploySha([])).toBeNull();
  });
});
