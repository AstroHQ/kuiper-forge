defmodule KuiperCoordinatorWeb.Admin.NotFound do
  @moduledoc "Raised by admin pages for an unknown agent: Phoenix sends it as a 404, like Rust's \"Agent not found\"."
  defexception message: "Agent not found", plug_status: 404
end
