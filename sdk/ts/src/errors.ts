export class Db9Error extends Error {
  readonly statusCode: number;
  readonly response?: Response;

  constructor(message: string, statusCode: number, response?: Response) {
    super(message);
    this.name = 'Db9Error';
    this.statusCode = statusCode;
    this.response = response;
  }

  static async fromResponse(response: Response): Promise<Db9Error> {
    // Parse { "message": string } body — the ONLY error format from the API
    let message: string;
    try {
      const body = (await response.json()) as { message?: string };
      message = body.message || response.statusText;
    } catch {
      message = response.statusText;
    }

    // Return specific subclass based on status code
    switch (response.status) {
      case 401:
        return new Db9AuthError(message, response);
      case 404:
        return new Db9NotFoundError(message, response);
      case 409:
        return new Db9ConflictError(message, response);
      default:
        return new Db9Error(message, response.status, response);
    }
  }
}

export class Db9AuthError extends Db9Error {
  constructor(message: string, response?: Response) {
    super(message, 401, response);
    this.name = 'Db9AuthError';
  }
}

export class Db9NotFoundError extends Db9Error {
  constructor(message: string, response?: Response) {
    super(message, 404, response);
    this.name = 'Db9NotFoundError';
  }
}

export class Db9ConflictError extends Db9Error {
  constructor(message: string, response?: Response) {
    super(message, 409, response);
    this.name = 'Db9ConflictError';
  }
}
