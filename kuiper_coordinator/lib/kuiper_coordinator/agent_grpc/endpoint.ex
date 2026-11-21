defmodule KuiperCoordinator.AgentGrpc.Endpoint do
  @moduledoc false
  use GRPC.Endpoint

  run(KuiperCoordinator.AgentGrpc.RegistrationServer)
  run(KuiperCoordinator.AgentGrpc.AgentServer)
end
