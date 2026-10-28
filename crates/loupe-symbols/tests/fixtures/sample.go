package server

import "fmt"

// MaxConns is the connection cap.
const MaxConns = 100

const internalLimit = 5

// Server handles requests.
type Server struct {
	Port int
}

// Handler processes a request.
type Handler interface {
	Handle(req string) string
}

type ID int

// New creates a Server.
func New(port int) *Server {
	const localConst = 1
	return &Server{Port: port}
}

// Run starts the server.
func (s *Server) Run() error {
	fmt.Println("run")
	return nil
}

func (s Server) helper() {}

func internalFunc() {}
