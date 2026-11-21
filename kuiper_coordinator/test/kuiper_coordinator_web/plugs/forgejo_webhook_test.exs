defmodule KuiperCoordinatorWeb.Plugs.ForgejoWebhookTest do
  # Forgejo 17 workflow_job_* webhooks (parity W4), driven by the real deliveries the Rust
  # coordinator's tests use (kuiper-forge/tests/fixtures/forgejo-17, tests/forgejo_webhook.rs).
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log

  alias KuiperCoordinator.Jobs
  alias KuiperCoordinatorWeb.Plugs.ForgejoWebhook

  @fixtures Path.expand("../../../../kuiper-forge/tests/fixtures/forgejo-17", __DIR__)
  # the secret the fixtures were signed with
  @secret "test-webhook-secret"
  @forge "fj"
  # handles of the fixtures' jobs: `build`, and `fails` (which `needs: build`)
  @build "b93ebf9d-946f-404a-a7f7-67bc8fc15e92"
  @fails "a7104b1c-328a-4de9-b4b5-e152c2807e70"
  @lodge {:org, "lodge"}

  setup do
    old = Application.get_env(:kuiper_coordinator, :webhook)
    configure()
    on_exit(fn -> Application.put_env(:kuiper_coordinator, :webhook, old || []) end)
    Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, "fleet")
    :ok
  end

  defp configure(opts \\ []) do
    Application.put_env(
      :kuiper_coordinator,
      :webhook,
      Keyword.merge(
        [path: "/webhook", secret: @secret, forge: @forge, source: :forgejo, scopes: [@lodge]],
        opts
      )
    )
  end

  defp fixture(name) do
    %{"headers" => headers, "body" => body} =
      Path.join(@fixtures, name <> ".json") |> File.read!() |> Jason.decode!()

    headers =
      headers
      |> Enum.reject(fn {k, _} -> String.downcase(k) in ["content-length", "host"] end)
      |> Map.new(fn {k, v} -> {String.downcase(k), v} end)

    %{headers: headers, body: body}
  end

  defp without(d, name), do: %{d | headers: Map.delete(d.headers, name)}
  defp with_header(d, name, value), do: %{d | headers: Map.put(d.headers, name, value)}

  defp deliver(d) do
    d.headers
    |> Enum.reduce(build_conn(), fn {k, v}, conn -> put_req_header(conn, k, v) end)
    |> post("/webhook", d.body)
    |> Map.fetch!(:status)
  end

  defp pending, do: Enum.map(Jobs.list_pending(), & &1.job_key)
  defp key(handle), do: "#{@forge}:#{handle}"

  test "the signature checks the real delivery's raw body" do
    d = fixture("waiting")
    sig = d.headers["x-forgejo-signature"]
    assert ForgejoWebhook.valid_signature?(@secret, d.body, sig)
    assert ForgejoWebhook.valid_signature?(@secret, d.body, String.upcase(sig))
    refute ForgejoWebhook.valid_signature?("wrong", d.body, sig)
    refute ForgejoWebhook.valid_signature?(@secret, d.body <> " ", sig)
    # GitHub's prefixed form isn't Forgejo's
    refute ForgejoWebhook.valid_signature?(@secret, d.body, "sha256=" <> sig)
    refute ForgejoWebhook.valid_signature?(@secret, d.body, "not hex")
    refute ForgejoWebhook.valid_signature?(@secret, d.body, "")
  end

  test "a waiting delivery queues one job keyed by its handle, with its details" do
    assert deliver(fixture("waiting")) == 200
    assert pending() == [key(@build)]
    assert_receive {:new_job, k} when k == "fj:" <> @build

    assert [job] = Jobs.list_pending()

    assert %{
             forge: @forge,
             runner_scope: @lodge,
             runner_group: nil,
             job_labels: ["self-hosted"],
             agent_labels: ["self-hosted"],
             repository: "lodge/app",
             job_name: "build",
             workflow_name: "ci.yml",
             queued_at: nil
           } = job

    # a redelivery keeps the one job
    assert deliver(fixture("waiting")) == 200
    assert pending() == [key(@build)]
  end

  test "the webhook and the polling scan share one job" do
    {:ok, :inserted} =
      Jobs.add_pending(%{
        job_key: key(@build),
        forge: @forge,
        job_labels: ["self-hosted"],
        agent_labels: ["self-hosted"],
        runner_scope: @lodge,
        repository: "lodge/app"
      })

    assert deliver(fixture("waiting")) == 200
    assert pending() == [key(@build)]
  end

  test "a bad, missing or GitHub-only signature is 401, and nothing is queued" do
    assert deliver(
             fixture("waiting")
             |> with_header("x-forgejo-signature", String.duplicate("0", 64))
           ) ==
             401

    assert deliver(fixture("waiting") |> without("x-forgejo-signature")) == 401

    # GitHub's header alone doesn't count for a Forgejo forge
    d = fixture("waiting")
    hub = d.headers["x-hub-signature-256"]

    assert deliver(d |> without("x-forgejo-signature") |> with_header("x-hub-signature-256", hub)) ==
             401

    tampered = fixture("waiting")
    tampered = %{tampered | body: String.replace(tampered.body, ~s("build"), ~s("bui1d"))}
    assert deliver(tampered) == 401
    assert pending() == []
  end

  test "other events get 200 before the signature is checked" do
    assert deliver(fixture("waiting") |> with_header("x-forgejo-event", "push")) == 200
    assert deliver(fixture("waiting") |> without("x-forgejo-event")) == 200
    assert pending() == []
  end

  test "a signed payload Forgejo's shape doesn't fit is 400, never a crash" do
    for body <- [
          "{not json",
          "[]",
          ~s({"action":"new_job_attempt"}),
          ~s({"action":"x","job":{"id":"1","name":"b","status":"waiting"},"run":{}}),
          ~s({"action":"x","job":{"id":1,"name":"b","status":"waiting","runs_on":[1]},"run":{"repository":{"full_name":"a/b","name":"b","owner":{"login":"a"}}}}),
          ~s({"action":"x","job":{"id":1,"name":"b","status":"waiting"},"run":{"repository":{"full_name":"a/b"}}})
        ] do
      sig = :crypto.mac(:hmac, :sha256, @secret, body) |> Base.encode16(case: :lower)

      d = %{
        headers: %{
          "x-forgejo-event" => "workflow_job_waiting",
          "x-forgejo-signature" => sig,
          "content-type" => "application/json"
        },
        body: body
      }

      assert deliver(d) == 400, body
    end

    assert pending() == []
  end

  test "a job without a handle (Forgejo < 15) is ignored" do
    body =
      ~s({"action":"new_job_attempt","job":{"id":1,"name":"b","status":"waiting","runs_on":["self-hosted"]},"run":{"repository":{"full_name":"lodge/app","name":"app","owner":{"login":"lodge"}}}})

    sig = :crypto.mac(:hmac, :sha256, @secret, body) |> Base.encode16(case: :lower)

    assert deliver(%{
             headers: %{
               "x-forgejo-event" => "workflow_job_waiting",
               "x-forgejo-signature" => sig,
               "content-type" => "application/json"
             },
             body: body
           }) == 200

    assert pending() == []
  end

  test "a blocked job waits for its waiting delivery" do
    assert deliver(fixture("blocked")) == 200
    assert pending() == []
    refute_received {:new_job, _}

    assert deliver(fixture("waiting-after-needs")) == 200
    assert pending() == [key(@fails)]
  end

  test "running and the finished states are announced to the fleet" do
    assert deliver(fixture("waiting")) == 200
    assert deliver(fixture("running")) == 200
    assert_receive {:job_started, k, nil} when k == "fj:" <> @build
    assert deliver(fixture("success")) == 200
    assert_receive {:job_completed, k} when k == "fj:" <> @build

    assert deliver(fixture("failure")) == 200
    assert_receive {:job_completed, k} when k == "fj:" <> @fails
    assert deliver(fixture("cancelled")) == 200
    assert_receive {:job_completed, "fj:33072997-d0d6-43a2-b989-694127d1012f"}
  end

  test "a job outside the forge's scopes is ignored" do
    configure(scopes: [{:repo, "kbot", "solo"}])
    assert deliver(fixture("waiting")) == 200
    assert pending() == []
  end

  test "the job's scope follows the configured scopes: repo, then owner, then instance" do
    configure(scopes: [:instance, @lodge, {:repo, "LODGE", "App"}])
    assert deliver(fixture("waiting")) == 200
    assert [%{runner_scope: {:repo, "LODGE", "App"}}] = Jobs.list_pending()

    assert ForgejoWebhook.job_scope([:instance, {:user, "Lodge"}], "lodge", "app") ==
             {:user, "Lodge"}

    assert ForgejoWebhook.job_scope([{:org, "x"}, :instance], "lodge", "app") == :instance

    assert ForgejoWebhook.job_scope([{:org, "x"}, {:repo, "lodge", "other"}], "lodge", "app") ==
             nil
  end

  test "the instance scope covers any repo" do
    configure(scopes: [:instance])
    assert deliver(fixture("waiting")) == 200
    assert [%{runner_scope: :instance}] = Jobs.list_pending()
  end

  test "required labels and label mappings apply" do
    configure(required_labels: ["linux"])
    assert deliver(fixture("waiting")) == 200
    assert pending() == []

    # a mapping for another forge never matches, so the job is ignored
    configure(
      label_mappings: [
        %{labels: ["self-hosted"], runner_scope: nil, runner_group: nil, forge: "github"}
      ]
    )

    assert deliver(fixture("waiting")) == 200
    assert pending() == []

    # a mapping for this forge picks the runner group, keeping the job's own scope
    configure(
      label_mappings: [
        %{labels: ["self-hosted"], runner_scope: nil, runner_group: "7", forge: @forge}
      ]
    )

    assert deliver(fixture("waiting")) == 200
    assert [%{runner_group: "7", runner_scope: @lodge}] = Jobs.list_pending()
  end

  test "a GitHub-source webhook ignores Forgejo deliveries" do
    configure(source: :github)
    assert deliver(fixture("waiting")) == 200
    assert pending() == []
  end
end
