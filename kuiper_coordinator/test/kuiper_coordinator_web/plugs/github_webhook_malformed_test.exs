defmodule KuiperCoordinatorWeb.Plugs.GitHubWebhookMalformedTest do
  # F6 (parity W2, from Nye): validly signed but malformed workflow_job payloads must get a 4xx, not a 500
  # (GitHub redelivers 5xx; a 503/500 loop on a bad payload never clears).
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log
  @secret "nye-review-secret"

  setup do
    old = Application.get_env(:kuiper_coordinator, :webhook)

    Application.put_env(:kuiper_coordinator, :webhook,
      path: "/webhook",
      secret: @secret,
      forge: "github",
      label_mappings: []
    )

    on_exit(fn -> Application.put_env(:kuiper_coordinator, :webhook, old || []) end)
    :ok
  end

  defp deliver(conn, map) do
    body = Jason.encode!(map)
    sig = "sha256=" <> (:crypto.mac(:hmac, :sha256, @secret, body) |> Base.encode16(case: :lower))

    conn
    |> put_req_header("content-type", "application/json")
    |> put_req_header("x-github-event", "workflow_job")
    |> put_req_header("x-hub-signature-256", sig)
    |> post("/webhook", body)
  end

  defp job(extra),
    do:
      Map.merge(
        %{"id" => 1, "labels" => ["self-hosted"], "created_at" => "2026-11-21T12:00:00Z"},
        extra
      )

  defp base(extra),
    do:
      Map.merge(
        %{
          "action" => "queued",
          "repository" => %{"full_name" => "acme/app", "owner" => %{"login" => "acme"}}
        },
        extra
      )

  for {name, payload} <- [
        {"workflow_job is a string", quote(do: base(%{"workflow_job" => "x"}))},
        {"a label is not a string",
         quote(do: base(%{"workflow_job" => job(%{"labels" => ["self-hosted", 5]})}))},
        {"created_at is a number",
         quote(do: base(%{"workflow_job" => job(%{"created_at" => 5})}))},
        {"no organization and no repository owner",
         quote(
           do: %{
             "action" => "queued",
             "workflow_job" => job(%{}),
             "repository" => %{"full_name" => "acme/app"}
           }
         )}
      ] do
    test "signed payload where #{name} gets a 4xx", %{conn: conn} do
      status =
        try do
          deliver(conn, unquote(payload)).status
        rescue
          e -> {:raised, e.__struct__}
        end

      assert is_integer(status) and status in 400..499, "got #{inspect(status)}"
    end
  end
end
