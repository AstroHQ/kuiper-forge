defmodule KuiperCoordinator.ForgesSecretLogTest do
  @moduledoc """
  Forge secrets (GitHub App key, Forgejo token, webhook secret) are encrypted at rest (D39), so
  Ecto's query log, even at debug in dev, must never print them (D79, same class as D65).
  Creating and updating forges of both kinds goes through queries with the secrets as
  parameters; none of them may show up.
  """
  use KuiperCoordinator.DataCase, async: false
  import ExUnit.CaptureLog
  require Logger

  alias KuiperCoordinator.Forges

  setup do
    level = Logger.level()
    Logger.configure(level: :debug)
    on_exit(fn -> Logger.configure(level: level) end)
    :ok
  end

  defp pem do
    key = :public_key.generate_key({:rsa, 2048, 65_537})
    :public_key.pem_encode([:public_key.pem_entry_encode(:RSAPrivateKey, key)])
  end

  test "debug logs of creating and updating forges never contain a secret" do
    key1 = pem()
    key2 = pem()

    secrets = [
      "fj-token-one-7f3a9",
      "fj-token-two-c81e2",
      "hook-secret-a1b2c3",
      "hook-secret-d4e5f6"
    ]

    [tok1, tok2, hook1, hook2] = secrets

    log =
      capture_log([level: :debug], fn ->
        {:ok, gh} =
          Forges.create_forge(
            %{
              name: "github",
              type: :github,
              app_id: 42,
              private_key: key1,
              webhook_secret: hook1
            },
            validate: false
          )

        {:ok, _} =
          Forges.update_forge(gh, %{private_key: key2, webhook_secret: hook2}, validate: false)

        {:ok, fj} =
          Forges.create_forge(
            %{
              name: "forgejo",
              type: :forgejo,
              url: "https://git.example.com/",
              token: tok1,
              scopes: ["acme"]
            },
            validate: false
          )

        {:ok, _} = Forges.update_forge(fj, %{token: tok2}, validate: false)
        _ = Forges.list_forges()
      end)

    # the capture really holds debug query logs (settings and reads are still logged)
    assert log =~ ~s(FROM "forges")

    for s <- secrets, do: refute(log =~ s, "debug log has secret #{s}")

    for k <- [key1, key2] do
      body = k |> String.split("\n") |> Enum.at(1)
      refute log =~ body, "debug log has a private key"
    end
  end
end
