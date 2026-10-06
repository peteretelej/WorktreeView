import { test } from "node:test";
import assert from "node:assert/strict";
import { errorCodeOf, errorMessage, isUnavailableProjectCode } from "./format.ts";

test("unavailable-project codes cover a deleted or de-git'd folder", () => {
  assert.equal(isUnavailableProjectCode("project_missing"), true);
  assert.equal(isUnavailableProjectCode("not_git_repository"), true);
  assert.equal(isUnavailableProjectCode("git_execution"), false);
  assert.equal(isUnavailableProjectCode(""), false);
});

test("unavailable-project errors carry the backend's explanation through", () => {
  const error = { code: "project_missing", message: "The folder C:\\tmp\\demo no longer exists on disk. It may have been deleted or moved." };
  assert.equal(errorMessage(error), error.message);
  assert.equal(errorCodeOf(error), "project_missing");
  assert.equal(isUnavailableProjectCode(errorCodeOf(error)), true);
});
