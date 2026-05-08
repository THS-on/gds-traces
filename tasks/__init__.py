from invoke import Collection

from . import docker, elbencho

ns = Collection()
ns.add_collection(docker.ns)
ns.add_collection(elbencho.ns)
