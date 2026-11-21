defmodule KuiperCoordinator.Admin.ApiTokensTest do
  @moduledoc "API tokens (parity UI9, api_tokens.rs): kfapi_ + 40 alnum, SHA-256 stored, 6-char prefix shown."
  use KuiperCoordinator.DataCase, async: true

  alias KuiperCoordinator.Admin.{ApiToken, ApiTokens}
  alias KuiperCoordinator.Repo

  test "create returns the plaintext once and stores only its hash" do
    {:ok, token, record} = ApiTokens.create("ci", "alice")
    assert token =~ ~r/\Akfapi_[A-Za-z0-9]{40}\z/
    assert record.token_prefix == binary_part(token, 0, 12)
    assert record.created_by == "alice"

    stored = Repo.get!(ApiToken, record.id)
    assert stored.token_hash == :crypto.hash(:sha256, token)
    refute inspect(stored) =~ token
  end

  test "names are 1-100 characters after trimming" do
    assert {:error, :invalid_name} = ApiTokens.create("   ", "alice")
    assert {:error, :invalid_name} = ApiTokens.create(String.duplicate("a", 101), "alice")
    assert {:ok, _, %{name: "ok"}} = ApiTokens.create("  ok ", "alice")
  end

  test "validate accepts a live token, bumps last_used_at, rejects others" do
    {:ok, token, record} = ApiTokens.create("ci", "alice")
    assert record.last_used_at == nil
    assert %ApiToken{id: id} = ApiTokens.validate(token)
    assert id == record.id
    assert Repo.get!(ApiToken, id).last_used_at

    assert ApiTokens.validate("kfapi_" <> String.duplicate("x", 40)) == nil
    # without the prefix it isn't even looked up
    assert ApiTokens.validate(String.replace_prefix(token, "kfapi_", "")) == nil
    assert ApiTokens.validate(nil) == nil
  end

  test "list is newest first; delete removes and then reports missing" do
    {:ok, _, a} = ApiTokens.create("a", "alice")
    {:ok, t, b} = ApiTokens.create("b", "alice")
    assert Enum.map(ApiTokens.list(), & &1.id) == [b.id, a.id]
    assert :ok = ApiTokens.delete(b.id)
    assert {:error, :not_found} = ApiTokens.delete(b.id)
    assert {:error, :not_found} = ApiTokens.delete("not-a-uuid")
    assert ApiTokens.validate(t) == nil
  end
end
