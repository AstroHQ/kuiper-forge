defmodule KuiperCoordinator.GitHub.Client404BodyTest do
  # F5 (parity G8, from Nye): GitHub's real 404 body carries "status": "404".
  use ExUnit.Case, async: true
  alias KuiperCoordinator.GitHub.Client

  setup do
    key = :public_key.generate_key({:rsa, 2048, 65_537})
    pem = :public_key.pem_encode([:public_key.pem_entry_encode(:RSAPrivateKey, key)])
    {:ok, cache} = Client.Cache.start_link()

    c =
      Client.new(
        app_id: 99,
        private_key: pem,
        cache: cache,
        req_options: [plug: {Req.Test, __MODULE__}, retry: false]
      )

    Req.Test.stub(__MODULE__, fn conn ->
      case {conn.method, conn.request_path} do
        {"GET", "/app/installations"} ->
          Req.Test.json(conn, [%{"id" => 7, "account" => %{"login" => "acme"}}])

        {"POST", "/app/installations/7/access_tokens"} ->
          exp = DateTime.utc_now() |> DateTime.add(3600) |> DateTime.to_iso8601()

          conn
          |> Plug.Conn.put_status(201)
          |> Req.Test.json(%{"token" => "ghs_x", "expires_at" => exp})

        {"GET", "/repos/acme/app/actions/jobs/2"} ->
          conn
          |> Plug.Conn.put_status(404)
          |> Req.Test.json(%{
            "message" => "Not Found",
            "documentation_url" =>
              "https://docs.github.com/rest/actions/workflow-jobs#get-a-job-for-a-workflow-run",
            "status" => "404"
          })
      end
    end)

    {:ok, client: c}
  end

  test "a real GitHub 404 job body reads as nil, not status \"404\"", %{client: c} do
    assert {:ok, nil} = Client.job_status(c, "acme/app", 2)
  end
end
