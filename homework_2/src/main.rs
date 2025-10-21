// Methods are added using the IMPL statement
struct Student {
    name: String,
    major: String,
}

impl Student {
    fn new(n: String, j: String) -> Student {
        Student {
            name: n,
            major: j,
        }
    }

    fn get_major(&self) -> &String {
        return &self.major
    }

    fn set_major(&mut self, new_major: String) {
        self.major = new_major;
    }
}

fn main() {


    let mut student = Student::new("Mariano".to_string(), "Computer Science".to_string());
    println!("Student name: {}", student.name);
    println!("Student major: {}", student.get_major());

    student.set_major("Mathematics".to_string());
    println!("Updated student major: {}", student.get_major());
}