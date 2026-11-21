defmodule KuiperCoordinator.Admin.User do
  @moduledoc "An admin UI user (Rust `admin/auth.rs` AdminUser); password is an Argon2id PHC hash."
  use Ecto.Schema

  @timestamps_opts [type: :utc_datetime_usec]
  schema "admin_users" do
    field :username, :string
    field :password_hash, :string, redact: true
    field :last_login_at, :utc_datetime_usec

    timestamps()
  end
end
