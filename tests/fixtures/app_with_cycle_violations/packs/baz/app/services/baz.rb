module Baz
  # Implicit reference to ::Foo: baz does not declare packs/foo as a
  # dependency, so this is a `dependency` violation. Adopting it explicitly
  # would close the cycle baz -> foo -> bar -> baz, so a `cycle` violation
  # is expected too.
  def self.calls_foo
    ::Foo
  end
end
