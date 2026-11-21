defmodule KuiperCoordinatorWeb.Plugs.ApiAuth do
  @moduledoc """
  Bearer `kfapi_` tokens for `/api/v1` (parity API1, admin/api.rs require_token). There's no
  cookie fallback: an admin session alone is a 401. Errors are JSON `{"error": ...}`.
  """
  import Plug.Conn

  alias KuiperCoordinator.Admin.ApiTokens

  def init(opts), do: opts

  def call(conn, _opts) do
    case bearer(conn) do
      nil ->
        conn
        |> put_resp_header("www-authenticate", "Bearer")
        |> error(401, "missing bearer token")

      token ->
        case ApiTokens.validate(token) do
          nil -> error(conn, 401, "invalid token")
          record -> assign(conn, :api_token, record)
        end
    end
  end

  defp bearer(conn) do
    case get_req_header(conn, "authorization") do
      ["Bearer " <> token | _] -> String.trim(token)
      _ -> nil
    end
  end

  def error(conn, status, message) do
    conn
    |> put_resp_content_type("application/json")
    |> send_resp(status, Jason.encode!(%{error: message}))
    |> halt()
  end
end
