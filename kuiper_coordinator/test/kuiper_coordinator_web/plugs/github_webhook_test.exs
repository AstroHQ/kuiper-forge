defmodule KuiperCoordinatorWeb.Plugs.GitHubWebhookTest do
  # GitHub workflow_job webhooks (parity W1-W3, W5-W7, J1)
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log

  alias KuiperCoordinator.Jobs

  @secret "It's a Secret to Everybody"

  setup do
    old = Application.get_env(:kuiper_coordinator, :webhook)

    Application.put_env(:kuiper_coordinator, :webhook,
      path: "/webhook",
      secret: @secret,
      forge: "github",
      label_mappings: [
        %{labels: ["self-hosted", "macOS"], runner_scope: nil, runner_group: "mac", forge: nil},
        %{
          labels: ["self-hosted", "linux"],
          runner_scope: {:repo, "acme", "infra"},
          runner_group: nil,
          forge: nil
        }
      ]
    )

    on_exit(fn -> Application.put_env(:kuiper_coordinator, :webhook, old || []) end)
    Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, "fleet")
    :ok
  end

  def sign(body, secret \\ @secret),
    do: "sha256=" <> (:crypto.mac(:hmac, :sha256, secret, body) |> Base.encode16(case: :lower))

  defp event(action, id, labels, extra \\ %{}) do
    Map.merge(
      %{
        "action" => action,
        "workflow_job" => %{
          "id" => id,
          "name" => "build",
          "labels" => labels,
          "created_at" => "2026-11-21T12:00:00Z",
          "runner_name" => "pve01-abcd1234"
        },
        "workflow_name" => "CI",
        "repository" => %{"full_name" => "acme/app", "owner" => %{"login" => "acme"}},
        "organization" => %{"login" => "acme-org"}
      },
      extra
    )
    |> Jason.encode!()
  end

  defp deliver(conn, body, opts \\ []) do
    conn
    |> put_req_header("content-type", "application/json")
    |> put_req_header("x-github-event", Keyword.get(opts, :event, "workflow_job"))
    |> put_req_header("x-hub-signature-256", Keyword.get_lazy(opts, :sig, fn -> sign(body) end))
    |> post("/webhook", body)
  end

  test "signature check matches GitHub's documented example" do
    assert KuiperCoordinatorWeb.Plugs.GitHubWebhook.valid_signature?(
             @secret,
             "Hello, World!",
             "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
           )

    refute KuiperCoordinatorWeb.Plugs.GitHubWebhook.valid_signature?(
             @secret,
             "Hello, World!",
             "757107ea"
           )

    refute KuiperCoordinatorWeb.Plugs.GitHubWebhook.valid_signature?(
             @secret,
             "Hello",
             "sha256=nothex!"
           )

    refute KuiperCoordinatorWeb.Plugs.GitHubWebhook.valid_signature?(@secret, "Hello", "")
  end

  test "other events get 200 before the signature is checked (W2)", %{conn: conn} do
    assert deliver(conn, "{}", event: "push", sig: "sha256=bad").status == 200
  end

  test "a bad signature is 401, checked over the raw body (W3)", %{conn: conn} do
    body = event("queued", 1, ["self-hosted", "macOS"])
    assert deliver(conn, body, sig: sign(body, "wrong")).status == 401
    # same JSON, different bytes: the signature is over the raw body, not re-encoded JSON
    assert deliver(build_conn(), " " <> body, sig: sign(body)).status == 401
    assert Jobs.list_pending() == []
  end

  test "bad JSON with a good signature is 400 (W2)", %{conn: conn} do
    assert deliver(conn, "{not json").status == 400
  end

  test "a queued job is persisted with its mapping, then the fleet is told (W7)", %{conn: conn} do
    body = event("queued", 42, ["self-hosted", "macOS", "ARM64"])
    assert deliver(conn, body).status == 200

    assert [job] = Jobs.list_pending()

    assert %{
             job_key: "github:42",
             forge: "github",
             job_labels: ["self-hosted", "macOS", "ARM64"],
             agent_labels: ["self-hosted", "macOS", "ARM64"],
             # mapping without a scope: the organization, else the repo owner
             runner_scope: {:org, "acme-org"},
             runner_group: "mac",
             repository: "acme/app",
             job_name: "build",
             workflow_name: "CI"
           } = job

    assert_received {:new_job, "github:42"}
  end

  test "the mapping's scope wins; repo owner when there's no organization", %{conn: conn} do
    assert deliver(conn, event("queued", 1, ["self-hosted", "linux"])).status == 200
    assert %{runner_scope: {:repo, "acme", "infra"}} = Jobs.get_pending("github:1")

    Application.put_env(
      :kuiper_coordinator,
      :webhook,
      Keyword.put(Application.get_env(:kuiper_coordinator, :webhook), :label_mappings, [])
    )

    body =
      event("queued", 2, ["self-hosted"])
      |> Jason.decode!()
      |> Map.delete("organization")
      |> Jason.encode!()

    assert deliver(build_conn(), body).status == 200
    assert %{runner_scope: {:org, "acme"}, runner_group: nil} = Jobs.get_pending("github:2")
  end

  test "a duplicate delivery makes one pending job (J1)", %{conn: conn} do
    body = event("queued", 7, ["self-hosted", "macOS"])
    assert deliver(conn, body).status == 200
    assert deliver(build_conn(), body).status == 200
    assert [%{job_key: "github:7"}] = Jobs.list_pending()
  end

  test "a job without self-hosted is ignored by default (W5, #4)", %{conn: conn} do
    assert deliver(conn, event("queued", 3, ["macOS"])).status == 200
    assert Jobs.list_pending() == []
    refute_received {:new_job, _}
  end

  test "a job no mapping matches is ignored (W6)", %{conn: conn} do
    assert deliver(conn, event("queued", 4, ["self-hosted", "windows"])).status == 200
    assert Jobs.list_pending() == []
  end

  test "completed and cancelled tell the fleet (W3)", %{conn: conn} do
    assert deliver(conn, event("completed", 5, ["self-hosted"])).status == 200
    assert_received {:job_completed, "github:5"}
    assert deliver(build_conn(), event("in_progress", 6, ["self-hosted"])).status == 200
    assert_received {:job_started, "github:6", "pve01-abcd1234"}
  end

  test "other paths are left alone", %{conn: conn} do
    assert get(conn, "/").status == 200
  end
end

defmodule KuiperCoordinatorWeb.Plugs.GitHubWebhookDbDownTest do
  # The webhook answers 503 when the job can't be written, so GitHub redelivers (J1, W2).
  # No sandbox checkout here: every Repo call fails like a database that's down.
  use ExUnit.Case, async: false
  @moduletag :capture_log
  import Plug.Conn
  import Phoenix.ConnTest
  @endpoint KuiperCoordinatorWeb.Endpoint

  setup do
    old = Application.get_env(:kuiper_coordinator, :webhook)

    Application.put_env(:kuiper_coordinator, :webhook,
      path: "/webhook",
      secret: "s",
      forge: "github"
    )

    on_exit(fn -> Application.put_env(:kuiper_coordinator, :webhook, old || []) end)
  end

  test "503 when the pending job can't be persisted" do
    body =
      Jason.encode!(%{
        "action" => "queued",
        "workflow_job" => %{"id" => 9, "name" => "b", "labels" => ["self-hosted"]},
        "repository" => %{"full_name" => "acme/app", "owner" => %{"login" => "acme"}}
      })

    conn =
      build_conn()
      |> put_req_header("content-type", "application/json")
      |> put_req_header("x-github-event", "workflow_job")
      |> put_req_header(
        "x-hub-signature-256",
        KuiperCoordinatorWeb.Plugs.GitHubWebhookTest.sign(body, "s")
      )
      |> post("/webhook", body)

    assert conn.status == 503
  end
end
