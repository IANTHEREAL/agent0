import { Entity, PrimaryGeneratedColumn, Column } from 'typeorm';

@Entity('typeorm_embeddings')
export class Embedding {
  @PrimaryGeneratedColumn()
  id!: number;

  @Column({ type: 'varchar', length: 100 })
  name!: string;

  @Column({ type: 'text' }) // Store vector as text since TypeORM doesn't have vector type
  embedding!: string;
}
