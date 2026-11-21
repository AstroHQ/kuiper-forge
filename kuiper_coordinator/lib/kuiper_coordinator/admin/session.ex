defmodule KuiperCoordinator.Admin.Session do
  @moduledoc "An admin login session: SHA-256 of the cookie's session id, fixed expiry (UI1)."
  use Ecto.Schema

  @timestamps_opts [type: :utc_datetime_usec, updated_at: false]
  schema "admin_sessions" do
    belongs_to :user, KuiperCoordinator.Admin.User
    field :token_hash, :binary, redact: true
    field :expires_at, :utc_datetime_usec
    field :ip_address, :string
    field :user_agent, :string

    timestamps()
  end
end
